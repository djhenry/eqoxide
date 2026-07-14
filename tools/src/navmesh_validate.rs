//! Offline MEASUREMENT HARNESS for the client-side navmesh bake (`eqoxide::navmesh`).
//!
//! Touches NO client pathing code, and the thing it measures is NOT shipping: the navmesh is not
//! wired under `Collision::find_path` and Phase 2 (doing so) is CANCELLED. This tool exists to
//! produce HONEST numbers about the representation — bake cost, cache size, oracle coverage, and an
//! A/B against the legacy grid — so that future decisions rest on measurements instead of vibes.
//!
//! # What this tool got WRONG before, and how it is prevented now
//!
//! An independent review found that every headline number this harness printed was inflated by a
//! measurement bug. They are listed here because a harness whose own history hides its wrong turns is
//! a harness nobody can trust:
//!
//!   1. **A grid TIMEOUT was bucketed as "the grid found no route."** `Collision::find_path` returns
//!      a bare `Option`, and the old code read `None` as "no path exists." It does not: it can also
//!      mean "I hit my budget and I don't know." 97% of the navmesh's claimed "462 routes the grid
//!      cannot find" were grid timeouts. That is the #337 anti-pattern — a timeout reported as a
//!      definitive "no" — inside the very tool meant to evaluate nav.
//!      **Now:** we call `find_path_ex`, which returns a `PlanOutcome` (`Route` / `Unreachable` /
//!      `Exhausted{limit}`), and an `Exhausted` grid answer gets its OWN bucket in every table.
//!      Parity is computed ONLY over pairs the grid answered definitively — an undecided pair cannot
//!      be scored for or against anyone.
//!   2. **A rise was called "unwalkable" by comparing it to `STEP_UP`.** `STEP_UP` (2.0) is a
//!      DISCRETE STEP height; the walker's SLOPE limit is `MAX_WALK_GRADE` (1.2). A 6.2u rise over an
//!      8u run is a 38° ramp it walks routinely. 125 of 238 flagged routes were ordinary ramps.
//!      **Now:** a segment is walkable if it is a small enough STEP *or* a shallow enough GRADE.
//!   3. **The "max 773u rise" headline was a bookkeeping artifact.** `assets.rs` snaps the final
//!      waypoint to the caller's exact goal z once the goal CELL is reached, so the last segment's
//!      Δz is the caller's own goal z, not a climb. 85 of the big rises sat on the final segment.
//!      **Now:** the goal-snap segment is EXCLUDED from the verdict and reported separately.
//!   4. **Endpoints were sampled from `mesh.populated_columns()` — the navmesh's OWN domain.** Ground
//!      the navmesh dropped could never be sampled, so the grid could never be credited for reaching
//!      it. Neutral sampling moved parity 93.88% → 83.5%. It also took `sa[0].z`, the LOWEST surface
//!      in a column, so upper tiers were never tested as endpoints.
//!      **Now:** endpoints are drawn from the EQEmu ORACLE's walkable polygon centroids — the
//!      declared ground truth, neutral to both planners — and each carries its own z, on its own tier.
//!   5. **`oracle_coverage` searched a ±1-cell XY ring and ±8u in z**, so it could not tell "we have
//!      this ground" from "we have something roughly near it": gfaydark reported 97.2% coverage while
//!      25% of the oracle's walkable XYs had no navmesh column at all.
//!      **Now:** three separate, labelled numbers — STRICT (same column), LOOSE (the old ring), and
//!      NO-COLUMN (the oracle has ground where we have nothing at all).
//!
//! # The oracle
//!
//! **EQEmu's prebuilt `maps/nav/*.nav`** — the GROUND-TRUTH ORACLE. This is the navmesh the EQEmu
//! server actually paths its NPCs on, in production, over the same geometry. We never ship or load
//! these in the client (navigation must stay a client-side computation); they are used here, offline,
//! purely to check that our bake did not lose walkable area the server believes exists, and to sample
//! A/B endpoints neutrally.
//!
//! It is a LOWER BOUND, not a ceiling: ground EQEmu omits may still be real.
//!
//! # Usage
//!
//! The oracle directory is NOT hardcoded (it is machine-specific, and this repo is public). Set it:
//!
//! ```text
//!   export EQOXIDE_NAVMESH_ORACLE_DIR=/path/to/eqemu/maps/nav
//!   navmesh_validate                          # every zone GLB present locally
//!   navmesh_validate qcat qeynos2             # named zones
//!   navmesh_validate --pairs 500              # A/B sample size (default 200)
//!   navmesh_validate --oracle-dir /path/...   # or pass it explicitly (wins over the env var)
//! ```

use anyhow::{Context, Result};
use eqoxide::assets::{Collision, PlanCtx, PlanOutcome, ZoneAssets};
use eqoxide::movement::{MAX_WALK_GRADE, PLAYER_RADIUS, STEP_UP};
use eqoxide::navmesh::{collision_tris, BakeParams, NavMesh};
use eqoxide::region_map::RegionMap;
use rand::{Rng, SeedableRng};
use std::time::Instant;

/// The env var naming EQEmu's `maps/nav` directory. There is deliberately NO default: the path is
/// machine-specific and this is a PUBLIC repository, so baking one in would leak a local filesystem
/// layout (it previously did). Absent the variable we fail loudly rather than guessing.
const ORACLE_ENV: &str = "EQOXIDE_NAVMESH_ORACLE_DIR";

// ─────────────── EQEmu .nav oracle (offline only — never linked into the client) ───────────────

struct OraclePoly {
    verts: Vec<[f32; 3]>,
}

impl OraclePoly {
    fn centroid(&self) -> [f32; 3] {
        let n = self.verts.len() as f32;
        [
            self.verts.iter().map(|v| v[0]).sum::<f32>() / n,
            self.verts.iter().map(|v| v[1]).sum::<f32>() / n,
            self.verts.iter().map(|v| v[2]).sum::<f32>() / n,
        ]
    }
}

/// Parse EQEmu's `EQNAVMESH` v2: zlib-deflated `[u32 tiles][dtNavMeshParams][tile…]`, standard
/// Detour v7 tiles. Format per EQEmu `zone/pathfinder_nav_mesh.cpp:403`.
fn load_oracle(path: &std::path::Path) -> Result<Vec<OraclePoly>> {
    let d = std::fs::read(path)?;
    anyhow::ensure!(d.len() > 21 && &d[..9] == b"EQNAVMESH", "not an EQNAVMESH file");
    let version = u32::from_le_bytes(d[9..13].try_into()?);
    anyhow::ensure!(version == 2, "unsupported .nav version {version}");
    let data_size = u32::from_le_bytes(d[13..17].try_into()?) as usize;
    let raw = miniz_oxide::inflate::decompress_to_vec_zlib(&d[21..21 + data_size])
        .map_err(|e| anyhow::anyhow!("inflate failed: {e:?}"))?;

    let mut o = 0usize;
    let ntiles = u32::from_le_bytes(raw[o..o + 4].try_into()?) as usize;
    o += 4;
    o += 28; // dtNavMeshParams

    let mut polys = Vec::new();
    for _ in 0..ntiles {
        o += 4; // tile_ref
        let tsize = u32::from_le_bytes(raw[o..o + 4].try_into()?) as usize;
        o += 4;
        let t = &raw[o..o + tsize];
        o += tsize;

        let ri = |b: usize| i32::from_le_bytes(t[b..b + 4].try_into().unwrap());
        let poly_count = ri(24) as usize;
        let vert_count = ri(28) as usize;

        const HDR: usize = 100; // dtMeshHeader
        let verts_off = HDR;
        let polys_off = verts_off + 12 * vert_count;
        let vert = |i: usize| -> [f32; 3] {
            let b = verts_off + 12 * i;
            let f = |k: usize| f32::from_le_bytes(t[b + k..b + k + 4].try_into().unwrap());
            // Detour stores (x, y=up, z) and EQEmu queries it as (eq_x, eq_z, eq_y), so world-space
            // is east = dt.x, north = dt.z, up = dt.y.
            [f(0), f(8), f(4)]
        };
        for p in 0..poly_count {
            let pb = polys_off + 32 * p; // dtPoly = 32 bytes
            let pv: Vec<u16> = (0..6)
                .map(|k| u16::from_le_bytes(t[pb + 4 + 2 * k..pb + 6 + 2 * k].try_into().unwrap()))
                .collect();
            let vcnt = t[pb + 30] as usize;
            let area_and_type = t[pb + 31];
            if area_and_type >> 6 == 1 { continue; } // DT_POLYTYPE_OFFMESH_CONNECTION
            if vcnt == 0 || vcnt > 6 { continue; }
            polys.push(OraclePoly { verts: (0..vcnt).map(|k| vert(pv[k] as usize)).collect() });
        }
    }
    Ok(polys)
}

/// How much of the oracle's walkable area our bake also has — reported THREE ways, because one number
/// cannot carry the distinction that matters.
///
/// The old single metric searched a ±1-cell XY ring at ±8u in z and reported ~97% for gfaydark, a
/// zone where a QUARTER of the oracle's walkable XYs have no navmesh column at all. A metric that
/// generous cannot tell "we have the ground" from "we have something roughly near it", which is
/// precisely the question the metric exists to answer.
#[derive(Clone, Copy, Default)]
struct Coverage {
    /// Same column, |dz| <= STRICT_Z. "We have THIS ground, at THIS height."
    strict: f32,
    /// ±1-cell ring, |dz| <= LOOSE_Z. The old, generous metric — kept for comparison, clearly labelled.
    loose: f32,
    /// The oracle has walkable ground at an XY where we have NO COLUMN AT ALL. The honest floor on
    /// how much we simply lost. This is the number that was invisible before.
    no_column: f32,
}

fn oracle_coverage(mesh: &NavMesh, oracle: &[OraclePoly]) -> Coverage {
    const STRICT_Z: f32 = 4.0;
    const LOOSE_Z: f32 = 8.0;
    if oracle.is_empty() { return Coverage::default(); }

    let (mut strict, mut loose, mut none) = (0usize, 0usize, 0usize);
    for p in oracle {
        let c = p.centroid();
        let (c0, r0) = mesh.to_cell(c[0], c[1]);

        if mesh.column(c0, r0).is_empty() { none += 1; }
        if mesh.column(c0, r0).iter().any(|s| (s.z - c[2]).abs() <= STRICT_Z) { strict += 1; }
        let hit_loose = (-1..=1).any(|dc| {
            (-1..=1).any(|dr| {
                mesh.column(c0 + dc, r0 + dr).iter().any(|s| (s.z - c[2]).abs() <= LOOSE_Z)
            })
        });
        if hit_loose { loose += 1; }
    }
    let n = oracle.len() as f32;
    Coverage {
        strict:    100.0 * strict as f32 / n,
        loose:     100.0 * loose as f32 / n,
        no_column: 100.0 * none as f32 / n,
    }
}

// ─────────────── route walkability, judged the way the WALKER actually moves ───────────────

/// The verdict on one grid route that the navmesh refused.
struct RouteVerdict {
    /// The steepest NON-goal-snap segment the walker could not traverse, if any. `None` = the walker
    /// really could have walked this route, so the navmesh genuinely missed it.
    worst_unwalkable: Option<Segment>,
    /// The biggest rise anywhere on the route EXCLUDING the goal-snap segment.
    max_rise: f32,
    /// True if the biggest rise on the WHOLE route sat on the final (goal-snap) segment — i.e. it is
    /// a bookkeeping artifact of `assets.rs` rewriting the last waypoint to the caller's goal z, not
    /// a climb the walker would ever attempt.
    max_rise_was_goal_snap: bool,
}

#[derive(Clone, Copy)]
struct Segment {
    rise: f32,
    run:  f32,
}

impl Segment {
    fn grade(&self) -> f32 {
        if self.run > 1e-3 { self.rise / self.run } else { f32::INFINITY }
    }
    /// Can the walker traverse this segment upward?
    ///
    /// TWO primitives, either of which suffices — this is the correction at the heart of the harness:
    ///   * a DISCRETE STEP: any rise up to `STEP_UP` (2.0), regardless of run. This is a ledge the
    ///     controller auto-steps.
    ///   * a RAMP: any rise whose GRADE (rise/run) is within `MAX_WALK_GRADE` (1.2, ~50°). A 6.2u
    ///     rise across an 8u cell is a 38° ramp — routine — even though 6.2 is far above `STEP_UP`.
    ///
    /// The old harness tested ONLY the first and so declared every ramp unwalkable. `STEP_UP` is a
    /// step height; it was never a slope limit.
    fn walkable(&self) -> bool {
        if self.rise <= 0.0 { return true; }              // descending / level: not a climb
        if self.rise <= STEP_UP { return true; }          // a step
        self.grade() <= MAX_WALK_GRADE                    // ...or a ramp
    }
}

/// Judge a grid route the way the CONTROLLER would have to walk it.
fn judge_route(start: [f32; 3], path: &[[f32; 3]]) -> RouteVerdict {
    let mut segs: Vec<Segment> = Vec::with_capacity(path.len());
    let mut prev = start;
    for w in path {
        segs.push(Segment {
            rise: w[2] - prev[2],
            run:  (w[0] - prev[0]).hypot(w[1] - prev[1]),
        });
        prev = *w;
    }

    // EXCLUDE the final segment. `assets.rs` snaps the last waypoint to the caller's exact goal
    // (x, y, z) once the goal CELL is reached, so its Δz is whatever z the caller passed in — it is
    // not a climb the walker plans or attempts. Scoring it produced the bogus "max 773u" headline.
    let body = if segs.is_empty() { &segs[..] } else { &segs[..segs.len() - 1] };

    let max_rise = body.iter().map(|s| s.rise).fold(f32::NEG_INFINITY, f32::max);
    let max_rise = if max_rise.is_finite() { max_rise } else { 0.0 };
    let whole_max = segs.iter().map(|s| s.rise).fold(f32::NEG_INFINITY, f32::max);
    let whole_max = if whole_max.is_finite() { whole_max } else { 0.0 };

    let worst_unwalkable = body.iter()
        .filter(|s| !s.walkable())
        .max_by(|a, b| a.grade().partial_cmp(&b.grade()).unwrap_or(std::cmp::Ordering::Equal))
        .copied();

    RouteVerdict {
        worst_unwalkable,
        max_rise,
        // The route's biggest rise is on the goal-snap segment and the body's is strictly smaller.
        max_rise_was_goal_snap: !segs.is_empty() && whole_max > max_rise + 1e-3,
    }
}

// ─────────────── per-zone run ───────────────

/// The A/B tally. The grid has THREE possible answers, not two — that distinction is the whole point.
#[derive(Default)]
struct AbTally {
    /// mesh routed, grid routed.
    both: usize,
    /// mesh routed, grid said DEFINITIVELY unreachable. A genuine navmesh advantage.
    mesh_only: usize,
    /// mesh routed, grid gave up (deadline / node cap). **NOT an advantage** — the grid never
    /// answered. Counting these as navmesh wins is what inflated "462 routes the grid cannot find".
    mesh_vs_grid_gaveup: usize,
    /// grid routed, mesh did not. The navmesh's misses.
    grid_only: usize,
    /// neither found a route, both definitively.
    neither: usize,
    /// mesh found nothing and the grid gave up: nobody knows anything. Scored for no one.
    both_unknown: usize,
}

impl AbTally {
    fn total(&self) -> usize {
        self.both + self.mesh_only + self.mesh_vs_grid_gaveup
            + self.grid_only + self.neither + self.both_unknown
    }
    /// Pairs on which the GRID gave a definitive answer. Only these can be scored: a pair the grid
    /// abandoned is undecided, and folding it into either column would be a lie about what we know.
    fn decided(&self) -> usize { self.both + self.mesh_only + self.grid_only + self.neither }
    /// Agreement-or-better, over the DECIDED pairs only.
    fn parity(&self) -> f32 {
        let d = self.decided();
        if d == 0 { return 0.0; }
        100.0 * (self.both + self.mesh_only + self.neither) as f32 / d as f32
    }
    fn gaveup(&self) -> usize { self.mesh_vs_grid_gaveup + self.both_unknown }
}

struct ZoneReport {
    zone:          String,
    tris:          usize,
    bake_ms:       u128,
    cache_bytes:   usize,
    columns:       usize,
    swim:          usize,
    stacked_pct:   f32,
    cov:           Option<Coverage>,
    ab:            AbTally,
    mesh_us_med:   u128,
    grid_us_med:   u128,
    mesh_us_max:   u128,
    grid_us_max:   u128,
    /// Every grid-only route, judged as the walker would have to walk it.
    verdicts:      Vec<RouteVerdict>,
    /// Diagnosis of each GENUINE miss (a walkable grid route the navmesh refused).
    misses:        Vec<String>,
}

fn median(v: &mut Vec<u128>) -> u128 {
    if v.is_empty() { return 0; }
    v.sort_unstable();
    v[v.len() / 2]
}

fn bake_zone(zone: &str, models: &std::path::Path, params: BakeParams)
    -> Result<(NavMesh, ZoneAssets, Option<RegionMap>, usize, u128, Vec<u8>)>
{
    let glb = models.join(format!("{zone}.glb"));
    let bytes = std::fs::read(&glb)?;
    let assets = ZoneAssets::from_glb(&glb)?;
    let tris = collision_tris(&assets);
    let water = RegionMap::load(&models.join("maps/water"), zone);

    let t0 = Instant::now();
    let mesh = NavMesh::bake(&tris, water.as_ref(), params, &bytes);
    let bake_ms = t0.elapsed().as_millis();
    Ok((mesh, assets, water, tris.len(), bake_ms, bytes))
}

fn run_zone(zone: &str, models: &std::path::Path, nav_dir: &std::path::Path,
            n_pairs: usize, params: BakeParams) -> Result<ZoneReport> {
    let (mesh, assets, water, tris, bake_ms, glb_bytes) = bake_zone(zone, models, params)?;

    // ── CACHE: size, round-trip, and — critically — that a changed source INVALIDATES it. Silently
    // pathing on a stale mesh would be exactly the class of lie this work exists to end.
    let blob = mesh.serialize();
    let cache_bytes = blob.len();
    let back = NavMesh::deserialize(&blob, &glb_bytes, params)
        .ok_or_else(|| anyhow::anyhow!("cache failed to round-trip"))?;
    anyhow::ensure!(back.surface_count() == mesh.surface_count(), "cache lost surfaces");
    anyhow::ensure!(NavMesh::deserialize(&blob, b"different-glb", params).is_none(),
        "STALE CACHE ACCEPTED — a changed GLB must force a re-bake");

    let mut cols_seen = 0usize;
    let mut stacked = 0usize;
    for (c, r) in mesh.populated_columns() {
        let col = mesh.column(c, r);
        cols_seen += 1;
        if col.windows(2).any(|w| w[1].z - w[0].z >= 12.0) { stacked += 1; }
    }

    // ── Oracle ──
    let oracle = load_oracle(&nav_dir.join(format!("{zone}.nav"))).ok()
        .filter(|o: &Vec<OraclePoly>| !o.is_empty());
    let cov = oracle.as_ref().map(|o| oracle_coverage(&mesh, o));

    // ── A/B vs the legacy grid ──
    let mut col = Collision::build(&assets, 32.0);
    col.set_water(water.map(std::sync::Arc::new));

    let mut ab = AbTally::default();
    let (mut m_us, mut g_us) = (Vec::new(), Vec::new());
    let mut verdicts: Vec<RouteVerdict> = Vec::new();
    let mut misses: Vec<String> = Vec::new();

    // ENDPOINTS ARE SAMPLED FROM THE ORACLE, NOT FROM OUR OWN MESH.
    //
    // The old harness drew both endpoints from `mesh.populated_columns()` — the navmesh's own domain.
    // That is a rigged sample in the navmesh's favour twice over: ground the navmesh DROPPED can never
    // be chosen, so the grid can never be credited for reaching it; and it took `sa[0].z`, the LOWEST
    // surface in the column, so upper tiers were never tested as endpoints at all. Correcting this
    // alone moved parity 93.88% → 83.5% and raised grid-only routes by 53%.
    //
    // The oracle's walkable polygon centroids are the PR's own declared ground truth and are neutral
    // to both planners — and each centroid carries its own z, on its own tier.
    //
    // No oracle → NO A/B. We do not fall back to self-sampling: a rigged number is worse than none.
    if let Some(o) = oracle.as_ref() {
        let mut rng = rand::rngs::StdRng::seed_from_u64(0xE0_0F);
        for _ in 0..n_pairs {
            let start = o[rng.gen_range(0..o.len())].centroid();
            let goal  = o[rng.gen_range(0..o.len())].centroid();

            let t = Instant::now();
            let mp = mesh.find_path(start, goal);
            m_us.push(t.elapsed().as_micros());

            // `find_path_ex`, NOT `find_path`. The bare `Option` from `find_path` cannot distinguish
            // "no route exists" from "I ran out of budget", and conflating them is precisely how this
            // harness manufactured a capability claim out of the grid's timeouts (#337).
            let t = Instant::now();
            let gp = col.find_path_ex(start, goal, PLAYER_RADIUS, &[], 8.0, None, 0.0,
                                      PlanCtx::default());
            g_us.push(t.elapsed().as_micros());

            match (mp.is_some(), &gp) {
                (true,  PlanOutcome::Route(_))          => ab.both += 1,
                (true,  PlanOutcome::Unreachable(_))    => ab.mesh_only += 1,
                (true,  PlanOutcome::Exhausted { .. })  => ab.mesh_vs_grid_gaveup += 1,
                (false, PlanOutcome::Unreachable(_))    => ab.neither += 1,
                (false, PlanOutcome::Exhausted { .. })  => ab.both_unknown += 1,
                (false, PlanOutcome::Route(p)) => {
                    ab.grid_only += 1;
                    let v = judge_route(start, p);
                    // A GENUINE miss: the walker really could have walked this grid route, and the
                    // navmesh refused it. These are the only true regressions in the parity data.
                    if v.worst_unwalkable.is_none() {
                        let why = match (mesh.nearest_index(start), mesh.nearest_index(goal)) {
                            (None, _) => "start does not anchor to any surface".to_string(),
                            (_, None) => "goal does not anchor to any surface".to_string(),
                            (Some(a), Some(b)) => {
                                let (sc, sa) = mesh.surface_at(a);
                                let (gc, ga) = mesh.surface_at(b);
                                format!("start comp={sc} (z={:.1}) goal comp={gc} (z={:.1}); {}",
                                    sa.z, ga.z,
                                    if sc != gc { "DIFFERENT COMPONENTS (no link)" }
                                    else { "same component (search failed?)" })
                            }
                        };
                        misses.push(format!("{zone}: {start:?} -> {goal:?} | {why}"));
                    }
                    verdicts.push(v);
                }
            }
        }
    }

    Ok(ZoneReport {
        zone: zone.into(), tris, bake_ms, cache_bytes,
        columns: cols_seen, swim: mesh.swim_surface_count(),
        stacked_pct: if cols_seen > 0 { 100.0 * stacked as f32 / cols_seen as f32 } else { 0.0 },
        cov,
        ab,
        mesh_us_med: median(&mut m_us.clone()), grid_us_med: median(&mut g_us.clone()),
        mesh_us_max: m_us.iter().copied().max().unwrap_or(0),
        grid_us_max: g_us.iter().copied().max().unwrap_or(0),
        verdicts,
        misses,
    })
}

// ─────────────── known failure cases (the acceptance tests) ───────────────

fn failure_cases(models: &std::path::Path, params: BakeParams) {
    println!("\n=== KNOWN FAILURE CASES ===");

    // #329 — qcat. The spawn corridor is flooded; the water surface (-56.00) is flush with the
    // CEILING (-55.97) and the real floor is -69.97. The legacy grid's `nearest_floor` has no normal
    // filter, so it anchors A* to the ceiling and plans the route across it.
    if let Ok((mesh, _, _, _, _, _)) = bake_zone("qcat", models, params) {
        let p = [-48.0, 1058.0, -66.0];
        match mesh.nearest_surface(p[0], p[1], p[2]) {
            Some(s) => println!("  #329 qcat spawn anchor {p:?} -> z={:.2}   [{}]", s.z,
                if s.z < -60.0 { "PASS (on the floor)" } else { "FAIL (anchored high — ceiling?)" }),
            None => println!("  #329 qcat spawn anchor -> NO SURFACE   [FAIL]"),
        }
        let (c, r) = mesh.to_cell(p[0], p[1]);
        let ceiling_is_walkable = mesh.column(c, r).iter()
            .any(|s| !s.is_swim() && (s.z - (-55.97)).abs() < 1.5);
        println!("  #329 qcat ceiling (-55.97) exposed as walkable? {ceiling_is_walkable}   [{}]",
            if ceiling_is_walkable { "FAIL" } else { "PASS" });
    }

    // #197p2 — the water-surface layer. The legacy grid has no waterline node at all, so a swimmer
    // is planned along the pool BOTTOM.
    for zone in ["halas", "qeynos2", "oasis"] {
        if let Ok((mesh, _, _, _, _, _)) = bake_zone(zone, models, params) {
            let swim = mesh.swim_surface_count();
            println!("  #197p2 {zone:<9} water-surface nodes: {swim:>6}   [{}]",
                if swim > 0 { "PASS" } else { "FAIL (no waterline layer)" });
        }
    }

    // Stacked geometry — the tiers must exist as multiple surfaces in one column.
    for zone in ["neriakc", "qcat", "blackburrow"] {
        if let Ok((mesh, _, _, _, _, _)) = bake_zone(zone, models, params) {
            let stacked = mesh.populated_columns()
                .filter(|&(c, r)| mesh.column(c, r).windows(2).any(|w| w[1].z - w[0].z >= 12.0))
                .count();
            println!("  stacked {zone:<9} columns with >=2 surfaces >=12u apart: {stacked:>6}   [{}]",
                if stacked > 0 { "PASS" } else { "FAIL" });
        }
    }
}

/// Resolve the oracle directory from `--oracle-dir` or `$EQOXIDE_NAVMESH_ORACLE_DIR`.
///
/// **No default, and no guessing.** This used to be a hardcoded absolute path into one developer's
/// container volume, committed to a PUBLIC repository. If it is not configured we say so and stop.
fn resolve_oracle_dir(args: &mut Vec<String>) -> Result<std::path::PathBuf> {
    let from_flag = args.iter().position(|a| a == "--oracle-dir").map(|i| {
        let v = args.get(i + 1).cloned();
        args.drain(i..=(i + 1).min(args.len() - 1));
        v
    });
    let dir = match from_flag {
        Some(Some(v)) => v,
        Some(None) => anyhow::bail!("--oracle-dir needs a path argument"),
        None => std::env::var(ORACLE_ENV).map_err(|_| anyhow::anyhow!(
            "the EQEmu navmesh oracle directory is not configured.\n\
             \n\
             This harness validates our bake against EQEmu's own prebuilt navmeshes (`maps/nav/*.nav`)\n\
             and samples its A/B endpoints from them. Point it at that directory:\n\
             \n\
             \x20   export {ORACLE_ENV}=/path/to/eqemu/maps/nav\n\
             \x20   navmesh_validate [zones...]\n\
             \n\
             ...or pass `--oracle-dir /path/to/eqemu/maps/nav`.\n\
             \n\
             (There is no built-in default on purpose: the path is machine-specific and this is a\n\
             public repository.)"))?,
    };
    let dir = std::path::PathBuf::from(dir);
    anyhow::ensure!(dir.is_dir(), "oracle dir does not exist or is not a directory: {}", dir.display());
    Ok(dir)
}

fn main() -> Result<()> {
    let models = dirs::data_dir()
        .context("no user data dir")?
        .join("eqoxide/assets/models");

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let nav_dir = resolve_oracle_dir(&mut args)?;

    let mut n_pairs = 200usize;
    if let Some(i) = args.iter().position(|a| a == "--pairs") {
        n_pairs = args[i + 1].parse()?;
        args.drain(i..=i + 1);
    }
    let mut params = BakeParams::default();
    if let Some(i) = args.iter().position(|a| a == "--cell") {
        params.cell_size = args[i + 1].parse()?;
        args.drain(i..=i + 1);
    }
    println!("bake params: {params:?}");
    println!("oracle: {} (offline ground truth; NOT shipped, NOT loaded by the client)", nav_dir.display());
    println!("A/B endpoints are sampled from the ORACLE, not from our own mesh — see the module docs.\n");

    let zones: Vec<String> = if args.is_empty() {
        let mut z: Vec<String> = std::fs::read_dir(&models)?
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().to_str().map(String::from))
            .filter(|n| n.ends_with(".glb") && !n.contains("_doors"))
            .map(|n| n.trim_end_matches(".glb").to_string())
            .collect();
        z.sort();
        z
    } else { args };

    // `gaveup` is its OWN column. It is not a navmesh win and not a grid loss — it is the grid
    // declining to answer, and it must never again be laundered into either.
    println!("{:<13}{:>7}{:>8}{:>9}{:>8}{:>7}{:>7} |{:>7}{:>7}{:>8} |{:>6}{:>6}{:>6}{:>6}{:>8} |{:>12}{:>12}",
        "zone", "tris", "bake_ms", "cache_kb", "columns", "swim", "stack%",
        "cov_str", "cov_lax", "no_col%",
        "both", "mesh", "grid", "none", "gaveup",
        "mesh_us", "grid_us");
    println!("{}", "-".repeat(152));

    let mut reports = Vec::new();
    for z in &zones {
        match run_zone(z, &models, &nav_dir, n_pairs, params) {
            Ok(r) => {
                let (cs, cl, cn) = match r.cov {
                    Some(c) => (format!("{:.1}", c.strict), format!("{:.1}", c.loose),
                                format!("{:.1}", c.no_column)),
                    None => ("-".into(), "-".into(), "-".into()),
                };
                println!("{:<13}{:>7}{:>8}{:>9.0}{:>8}{:>7}{:>6.1}% |{:>7}{:>7}{:>8} |{:>6}{:>6}{:>6}{:>6}{:>8} |{:>12}{:>12}",
                    r.zone, r.tris, r.bake_ms, r.cache_bytes as f32 / 1024.0, r.columns,
                    r.swim, r.stacked_pct,
                    cs, cl, cn,
                    r.ab.both, r.ab.mesh_only, r.ab.grid_only, r.ab.neither, r.ab.gaveup(),
                    format!("{}/{}", r.mesh_us_med, r.mesh_us_max),
                    format!("{}/{}", r.grid_us_med, r.grid_us_max));
                reports.push(r);
            }
            Err(e) => println!("{z:<13} ERROR: {e}"),
        }
    }

    if reports.is_empty() { return Ok(()); }

    let n = reports.len();
    let bakes: Vec<u128> = reports.iter().map(|r| r.bake_ms).collect();
    let cache: usize = reports.iter().map(|r| r.cache_bytes).sum();

    let mut tot = AbTally::default();
    for r in &reports {
        tot.both += r.ab.both;
        tot.mesh_only += r.ab.mesh_only;
        tot.mesh_vs_grid_gaveup += r.ab.mesh_vs_grid_gaveup;
        tot.grid_only += r.ab.grid_only;
        tot.neither += r.ab.neither;
        tot.both_unknown += r.ab.both_unknown;
    }

    println!("\n=== TOTALS ({n} zones) ===");
    println!("BAKE TIME (the client's zone-load cost): median {} ms | max {} ms ({})",
        median(&mut bakes.clone()), bakes.iter().max().unwrap(),
        reports.iter().max_by_key(|r| r.bake_ms).unwrap().zone);

    // Extrapolating a whole-cache size needs a DENOMINATOR, and the denominator is the zone universe
    // — not however many zones happened to be baked locally today. Count it, don't guess it: a
    // previous version of this PR asserted "~180 MB for ~200 zones" with no source for the 200.
    let mean_kb = cache as f32 / 1024.0 / n as f32;
    let universe = std::fs::read_dir(&nav_dir).map(|d| {
        d.filter_map(|e| e.ok())
         .filter(|e| e.path().extension().is_some_and(|x| x == "nav"))
         .count()
    }).unwrap_or(0);
    println!("CACHE SIZE: total {:.1} MB over {n} zones | mean {mean_kb:.0} KB/zone | max {:.0} KB ({})",
        cache as f32 / 1048576.0,
        reports.iter().map(|r| r.cache_bytes).max().unwrap() as f32 / 1024.0,
        reports.iter().max_by_key(|r| r.cache_bytes).unwrap().zone);
    if universe > 0 {
        println!("  -> full-cache extrapolation: {universe} zones x {mean_kb:.0} KB = {:.0} MB \
                  (zone count from {} — measured, not assumed)",
            universe as f32 * mean_kb / 1024.0, nav_dir.display());
    }

    let mut all_m: Vec<u128> = reports.iter().map(|r| r.mesh_us_med).collect();
    let mut all_g: Vec<u128> = reports.iter().map(|r| r.grid_us_med).collect();
    println!("QUERY TIME: navmesh median {} us (worst {} us) | legacy grid median {} us (worst {} us)",
        median(&mut all_m), reports.iter().map(|r| r.mesh_us_max).max().unwrap(),
        median(&mut all_g), reports.iter().map(|r| r.grid_us_max).max().unwrap());
    println!("  NOTE: do NOT quote a speedup ratio off these. If the grid is running under a deadline,\n\
              \x20       its time is CENSORED at the budget — that is a CAP, not a COST, and dividing by\n\
              \x20       it manufactures a speedup. Compare only on pairs where neither planner is\n\
              \x20       truncated. The navmesh's own worst case is UNBOUNDED (no deadline, no node cap).");

    let covs: Vec<Coverage> = reports.iter().filter_map(|r| r.cov).collect();
    if !covs.is_empty() {
        let m = |f: fn(&Coverage) -> f32| covs.iter().map(f).sum::<f32>() / covs.len() as f32;
        println!("ORACLE COVERAGE (our bake vs EQEmu's own navmesh), three ways:");
        println!("  STRICT  (same column, |dz|<=4u)  mean {:.1}%   <- 'we have THIS ground'", m(|c| c.strict));
        println!("  LOOSE   (+-1 cell, |dz|<=8u)     mean {:.1}%   <- the old, generous metric", m(|c| c.loose));
        println!("  NO COLUMN AT ALL                 mean {:.1}%   <- oracle has ground, we have NOTHING",
            m(|c| c.no_column));
    }

    println!("\n=== GO / NO-GO ===");
    println!("A/B pairs: {}   both={}  mesh-only={}  grid-only={}  neither={}",
        tot.total(), tot.both, tot.mesh_only, tot.grid_only, tot.neither);
    println!("GRID DECLINED TO ANSWER on {} pairs ({:.1}%) — deadline or node cap.",
        tot.gaveup(), 100.0 * tot.gaveup() as f32 / tot.total().max(1) as f32);
    println!("  These are UNDECIDED and are excluded from parity. They are NOT navmesh wins: reporting\n\
              \x20 a timeout as 'a route the grid cannot find' is the #337 lie, and it is how this\n\
              \x20 harness once claimed 462 such routes when 97% of them were grid timeouts.");

    let parity = tot.parity();
    println!("parity-or-better reachability (over the {} DECIDED pairs): {parity:.2}%   (gate: >= 95%)  ->  {}",
        tot.decided(),
        if parity >= 95.0 { "PROCEED" } else { "DO NOT PROCEED — fall back to patching the grid" });

    // Are those grid-only routes REAL? Judge them as the WALKER moves — a step OR a ramp — not
    // against STEP_UP alone, and never on the goal-snap segment.
    if !reports.iter().all(|r| r.verdicts.is_empty()) {
        let vs: Vec<&RouteVerdict> = reports.iter().flat_map(|r| r.verdicts.iter()).collect();
        let unwalkable = vs.iter().filter(|v| v.worst_unwalkable.is_some()).count();
        let genuine = vs.len() - unwalkable;
        let snap_artifacts = vs.iter().filter(|v| v.max_rise_was_goal_snap).count();

        println!("\n  --- the {} grid-only routes, judged as the WALKER would walk them ---", vs.len());
        println!("  GENUINE navmesh misses (the walker really could have walked it): {genuine} ({:.1}% of decided)",
            100.0 * genuine as f32 / tot.decided().max(1) as f32);
        println!("  truly unwalkable by the grid (a step > STEP_UP({STEP_UP}u) AND a grade > MAX_WALK_GRADE({MAX_WALK_GRADE})): {unwalkable}");
        println!("  routes whose biggest rise sat on the GOAL-SNAP segment (a bookkeeping artifact of\n\
                  \x20   assets.rs rewriting the last waypoint to the caller's goal z, NOT a climb): {snap_artifacts}");

        let mut rises: Vec<f32> = vs.iter().map(|v| v.max_rise).collect();
        rises.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        if !rises.is_empty() {
            println!("  max mid-route rise (goal-snap EXCLUDED): median {:.1}u, p90 {:.1}u, max {:.1}u",
                rises[rises.len() / 2], rises[rises.len() * 9 / 10], rises[rises.len() - 1]);
        }

        println!("\n  --- ROOT CAUSE of each genuine miss ---");
        for m in reports.iter().flat_map(|r| r.misses.iter()) { println!("    {m}"); }

        // The old harness printed an "adjusted parity" that added every STEP_UP-flagged route back
        // into the numerator, reaching 99.18% and declaring PROCEED. That was the GATE REDEFINED TO
        // PASS: most of those routes were ordinary ramps. Discounting is only honest for routes the
        // walker provably cannot walk, and it is reported as a clearly-labelled SECONDARY number.
        let adj_num = tot.both + tot.mesh_only + tot.neither + unwalkable;
        let adj = 100.0 * adj_num as f32 / tot.decided().max(1) as f32;
        println!("\n  [secondary] parity discounting ONLY the provably-unwalkable grid routes: {adj:.2}%");
        println!("  This is NOT the gate. The gate is the {parity:.2}% above.");
    }

    failure_cases(&models, params);
    Ok(())
}
