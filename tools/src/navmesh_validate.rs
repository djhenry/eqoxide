//! Offline validation harness for the client-side navmesh bake (`eqoxide::navmesh`).
//!
//! Touches NO client pathing code. It bakes a navmesh from each zone's own collision mesh — exactly
//! as the client will at zone load — and validates it against three references:
//!
//!   1. **EQEmu's prebuilt `maps/nav/*.nav`** — the GROUND-TRUTH ORACLE. This is the navmesh the
//!      EQEmu server actually paths its NPCs on, in production, over the same geometry. We never
//!      ship or load these in the client (navigation must stay a client-side computation); they are
//!      used here, offline, purely to check that our bake did not lose walkable area the server
//!      believes exists. A low coverage number means OUR bake parameters are wrong.
//!   2. **The legacy grid** (`Collision::find_path`) — A/B over random start/goal pairs: reachability
//!      agreement, and wall-clock per query.
//!   3. **The known failure cases** (#329, #197p2, stacked tiers) — the acceptance tests.
//!
//! It also measures the three numbers that decide the design: BAKE TIME (the client's zone-load
//! cost), CACHE SIZE, and QUERY TIME.
//!
//! Usage:
//!   navmesh_validate                 # every zone GLB present locally
//!   navmesh_validate qcat qeynos2    # named zones
//!   navmesh_validate --pairs 500     # A/B sample size (default 200)

use anyhow::Result;
use eqoxide::assets::{Collision, ZoneAssets};
use eqoxide::navmesh::{collision_tris, BakeParams, NavMesh};
use eqoxide::region_map::RegionMap;
use rand::{Rng, SeedableRng};
use std::time::Instant;

// ─────────────── EQEmu .nav oracle (offline only — never linked into the client) ───────────────

struct OraclePoly {
    verts: Vec<[f32; 3]>,
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

/// Fraction of the oracle's walkable polygons our bake also considers walkable: for each oracle
/// polygon centroid, is there a surface of ours within `tol` in z, in that XY neighbourhood?
fn oracle_coverage(mesh: &NavMesh, oracle: &[OraclePoly], tol: f32) -> (usize, usize) {
    let mut covered = 0;
    for p in oracle {
        let n = p.verts.len() as f32;
        let c = [
            p.verts.iter().map(|v| v[0]).sum::<f32>() / n,
            p.verts.iter().map(|v| v[1]).sum::<f32>() / n,
            p.verts.iter().map(|v| v[2]).sum::<f32>() / n,
        ];
        let (c0, r0) = mesh.to_cell(c[0], c[1]);
        let hit = (-1..=1).any(|dc| {
            (-1..=1).any(|dr| mesh.column(c0 + dc, r0 + dr).iter().any(|s| (s.z - c[2]).abs() <= tol))
        });
        if hit { covered += 1; }
    }
    (covered, oracle.len())
}

// ─────────────── per-zone run ───────────────

struct ZoneReport {
    zone:          String,
    tris:          usize,
    bake_ms:       u128,
    cache_bytes:   usize,
    columns:       usize,
    surfaces:      usize,
    swim:          usize,
    stacked_pct:   f32,
    oracle_cov:    Option<f32>,
    /// % of OUR surfaces EQEmu's NPC mesh has no polygon for — player-only terrain candidates.
    player_only:   Option<f32>,
    pairs:         usize,
    both_ok:       usize,
    mesh_only:     usize,
    grid_only:     usize,
    neither:       usize,
    mesh_us_med:   u128,
    grid_us_med:   u128,
    mesh_us_max:   u128,
    grid_us_max:   u128,
    /// Biggest single rise on each route the GRID found but the navmesh refused. A rise above
    /// movement::STEP_UP (2.0) means the walker could never have walked that grid route anyway.
    g_only_rise:   Vec<f32>,
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

    // ── Inverse coverage: ground WE have that EQEmu does NOT (player-only terrain candidates).
    // Reported, never pruned. EQEmu's mesh is an NPC mesh; a player reaches places a mob never does.
    let player_only = match load_oracle(&nav_dir.join(format!("{zone}.nav"))) {
        Ok(o) if !o.is_empty() => {
            // Bucket EQEmu polys by XY cell. Index each poly's whole AABB, not just its centroid:
            // Detour polys are LARGE and near-planar, so a centroid-only index would report our
            // surfaces in a big poly's INTERIOR as "player-only" and wildly overstate this number.
            // Using the AABB over-covers slightly, which errs toward UNDER-counting player-only —
            // the honest direction for a claim of the form "EQEmu has no ground here".
            let mut idx: std::collections::HashMap<(i32, i32), Vec<f32>> = Default::default();
            for p in &o {
                let n = p.verts.len() as f32;
                let cz = p.verts.iter().map(|v| v[2]).sum::<f32>() / n;
                let (mut e0, mut e1) = (f32::MAX, f32::MIN);
                let (mut n0, mut n1) = (f32::MAX, f32::MIN);
                for v in &p.verts {
                    e0 = e0.min(v[0]); e1 = e1.max(v[0]);
                    n0 = n0.min(v[1]); n1 = n1.max(v[1]);
                }
                let (b0, b1) = ((e0 / 8.0).floor() as i32, (e1 / 8.0).floor() as i32);
                let (c0, c1) = ((n0 / 8.0).floor() as i32, (n1 / 8.0).floor() as i32);
                for bx in b0..=b1 {
                    for by in c0..=c1 {
                        idx.entry((bx, by)).or_default().push(cz);
                    }
                }
            }
            let (mut ours, mut only) = (0usize, 0usize);
            // Break the player-only ground down by WHY an NPC mesh would lack it. Swim surfaces and
            // descend-only steep faces are things a player can use and a roaming mob cannot — those
            // are the expected, legitimate divergence. Plain WALKABLE ground EQEmu lacks is the
            // interesting bucket (ledges/rooftops/interiors mobs never visit).
            let (mut po_swim, mut po_steep, mut po_walk) = (0usize, 0usize, 0usize);
            for (c, r) in mesh.populated_columns() {
                let ctr = mesh.center(c, r);
                let key = ((ctr[0] / 8.0).floor() as i32, (ctr[1] / 8.0).floor() as i32);
                for s in mesh.column(c, r) {
                    ours += 1;
                    let hit = (-1..=1).any(|dc| (-1..=1).any(|dr| {
                        idx.get(&(key.0 + dc, key.1 + dr))
                           .is_some_and(|zs| zs.iter().any(|z| (z - s.z).abs() <= 8.0))
                    }));
                    if !hit {
                        only += 1;
                        if s.is_swim() { po_swim += 1; }
                        else if s.flags & eqoxide::navmesh::FLAG_STEEP != 0 { po_steep += 1; }
                        else { po_walk += 1; }
                    }
                }
            }
            if only > 0 {
                // Of the ground EQEmu lacks, how much is REACHABLE from the main walkable world?
                // Unreachable ground (a wall top my 2u cells happen to surface, a sealed ledge) is
                // inert: it sits in its own component and A* can never route onto it. Only the
                // reachable part is a genuine player-only feature.
                let main = mesh.main_component();
                let reach_only = mesh.iter_surfaces().filter(|(c, _)| *c == main).count();
                println!("  [player-only {zone}] {:.1}% of our ground has no EQEmu polygon \
                    (swim {:.0}%, steep {:.0}%, plain {:.0}%); \
                    main walkable component holds {} of {} surfaces ({:.0}%)",
                    100.0 * only as f32 / ours.max(1) as f32,
                    100.0 * po_swim as f32 / only as f32,
                    100.0 * po_steep as f32 / only as f32,
                    100.0 * po_walk as f32 / only as f32,
                    reach_only, ours, 100.0 * reach_only as f32 / ours.max(1) as f32);
            }
            Some(100.0 * only as f32 / ours.max(1) as f32)
        }
        _ => None,
    };

    // ── Oracle ──
    let oracle_cov = match load_oracle(&nav_dir.join(format!("{zone}.nav"))) {
        Ok(o) if !o.is_empty() => {
            let (cov, tot) = oracle_coverage(&mesh, &o, 8.0);
            let pct = 100.0 * cov as f32 / tot as f32;
            // A low number means our bake and EQEmu's disagree badly. Before blaming the baker,
            // check whether the two are even describing the same volume of world: if the GLB we
            // ship and the .map EQEmu baked from cover different bounds, this is an ASSET mismatch,
            // not a bake bug, and the coverage number is meaningless for that zone.
            if pct < 60.0 {
                let bb = |pts: &mut dyn Iterator<Item = [f32; 3]>| {
                    let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                    for p in pts { for k in 0..3 { lo[k] = lo[k].min(p[k]); hi[k] = hi[k].max(p[k]); } }
                    (lo, hi)
                };
                let (olo, ohi) = bb(&mut o.iter().flat_map(|p| p.verts.iter().copied()));
                let (mlo, mhi) = bb(&mut mesh.populated_columns().flat_map(|(c, r)| {
                    let ctr = mesh.center(c, r);
                    mesh.column(c, r).iter().map(move |s| [ctr[0], ctr[1], s.z]).collect::<Vec<_>>()
                }));
                println!("  [diag {zone}] LOW COVERAGE {pct:.1}% — bounds check:");
                println!("      ours   e[{:.0}..{:.0}] n[{:.0}..{:.0}] z[{:.0}..{:.0}]",
                    mlo[0], mhi[0], mlo[1], mhi[1], mlo[2], mhi[2]);
                println!("      EQEmu  e[{:.0}..{:.0}] n[{:.0}..{:.0}] z[{:.0}..{:.0}]",
                    olo[0], ohi[0], olo[1], ohi[1], olo[2], ohi[2]);
                // Decisive: at an EQEmu poly's XY, do we have NO surface (missing geometry) or a
                // surface at a DIFFERENT z (offset / wrong tier)?
                let (mut empty, mut zdiff, mut ok) = (0, 0, 0);
                let mut samples = Vec::new();
                for op in o.iter().take(4000) {
                    let n = op.verts.len() as f32;
                    let c = [op.verts.iter().map(|v| v[0]).sum::<f32>() / n,
                             op.verts.iter().map(|v| v[1]).sum::<f32>() / n,
                             op.verts.iter().map(|v| v[2]).sum::<f32>() / n];
                    let (cc, rr) = mesh.to_cell(c[0], c[1]);
                    let col = mesh.column(cc, rr);
                    if col.is_empty() { empty += 1; }
                    else if col.iter().any(|s| (s.z - c[2]).abs() <= 8.0) { ok += 1; }
                    else {
                        zdiff += 1;
                        if samples.len() < 3 {
                            samples.push(format!("EQEmu z={:.1} vs ours {:?}", c[2],
                                col.iter().map(|s| format!("{:.1}", s.z)).collect::<Vec<_>>()));
                        }
                    }
                }
                println!("      at EQEmu poly XYs: no-surface={empty}  z-mismatch={zdiff}  match={ok}");
                // Is the mismatch a SYSTEMATIC vertical offset (our asset baked at the wrong height)
                // or scattered noise (genuinely different geometry)? A tight cluster means offset.
                let mut dzs: Vec<f32> = Vec::new();
                for op in o.iter().take(6000) {
                    let n = op.verts.len() as f32;
                    let c = [op.verts.iter().map(|v| v[0]).sum::<f32>() / n,
                             op.verts.iter().map(|v| v[1]).sum::<f32>() / n,
                             op.verts.iter().map(|v| v[2]).sum::<f32>() / n];
                    let (cc, rr) = mesh.to_cell(c[0], c[1]);
                    let col = mesh.column(cc, rr);
                    if col.is_empty() { continue; }
                    // nearest of OUR surfaces to EQEmu's height
                    let best = col.iter().map(|s| s.z - c[2])
                        .min_by(|a, b| a.abs().partial_cmp(&b.abs()).unwrap()).unwrap();
                    dzs.push(best);
                }
                if !dzs.is_empty() {
                    dzs.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    let med = dzs[dzs.len()/2];
                    let p10 = dzs[dzs.len()/10];
                    let p90 = dzs[dzs.len()*9/10];
                    let within8 = dzs.iter().filter(|d| d.abs() <= 8.0).count();
                    println!("      dz(ours - EQEmu) at shared XYs: p10={p10:.1} median={med:.1} p90={p90:.1}  |dz|<=8u: {:.0}%",
                        100.0 * within8 as f32 / dzs.len() as f32);
                    println!("      -> {}", if (p90 - p10).abs() < 20.0 {
                        "TIGHT spread = a SYSTEMATIC vertical offset (asset baked at the wrong height)"
                    } else {
                        "WIDE spread = not a simple offset; genuinely different geometry"
                    });
                }
                for s in &samples { println!("        {s}"); }
            }
            Some(pct)
        }
        _ => None,
    };

    // ── A/B vs the legacy grid ──
    let mut col = Collision::build(&assets, 32.0);
    col.set_water(water.map(std::sync::Arc::new));

    let all: Vec<(i32, i32)> = mesh.populated_columns().collect();
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xE0_0F);
    let (mut both, mut m_only, mut g_only, mut none_) = (0, 0, 0, 0);
    let (mut m_us, mut g_us) = (Vec::new(), Vec::new());
    let mut g_only_rise: Vec<f32> = Vec::new();
    let mut misses: Vec<String> = Vec::new();
    if all.len() >= 2 {
        for _ in 0..n_pairs {
            let a = all[rng.gen_range(0..all.len())];
            let b = all[rng.gen_range(0..all.len())];
            let (sa, sb) = (mesh.column(a.0, a.1), mesh.column(b.0, b.1));
            if sa.is_empty() || sb.is_empty() { continue; }
            let (pa, pb) = (mesh.center(a.0, a.1), mesh.center(b.0, b.1));
            let start = [pa[0], pa[1], sa[0].z];
            let goal  = [pb[0], pb[1], sb[0].z];

            let t = Instant::now();
            let mp = mesh.find_path(start, goal);
            m_us.push(t.elapsed().as_micros());

            let t = Instant::now();
            let gp = col.find_path(start, goal, eqoxide::movement::PLAYER_RADIUS, &[], false);
            g_us.push(t.elapsed().as_micros());

            match (mp.is_some(), gp.as_ref()) {
                (true, Some(_))  => both += 1,
                (true, None)     => m_only += 1,
                (false, Some(p)) => {
                    g_only += 1;
                    // Root-cause the GENUINE misses (a grid route the walker really could walk that
                    // the navmesh refuses). These are the only true regressions in the parity data.
                    let mut mr: f32 = 0.0;
                    let mut pv = start;
                    for w in p.iter() { mr = mr.max(w[2] - pv[2]); pv = *w; }
                    if mr <= eqoxide::movement::STEP_UP {
                        let si = mesh.nearest_index(start);
                        let gi = mesh.nearest_index(goal);
                        let why = match (si, gi) {
                            (None, _) => "start does not anchor to any surface".to_string(),
                            (_, None) => "goal does not anchor to any surface".to_string(),
                            (Some(a), Some(b)) => {
                                let (sc, sa) = mesh.surface_at(a);
                                let (gc, ga) = mesh.surface_at(b);
                                // How far did the grid route have to travel vertically / did it use a
                                // horizontal GAP (a jump edge, which our mesh has no equivalent for)?
                                let mut maxgap: f32 = 0.0;
                                let mut pv = start;
                                for w in p.iter() {
                                    maxgap = maxgap.max((w[0]-pv[0]).hypot(w[1]-pv[1]));
                                    pv = *w;
                                }
                                format!("start comp={sc} (z={:.1}) goal comp={gc} (z={:.1}); {}; grid max step {:.0}u",
                                    sa.z, ga.z,
                                    if sc != gc { "DIFFERENT COMPONENTS (no link)" } else { "same component (search failed?)" },
                                    maxgap)
                            }
                        };
                        misses.push(format!("{}: {:?} -> {:?} | {why}", zone, start, goal));
                    }
                    // IS THAT GRID ROUTE ACTUALLY WALKABLE? The legacy A* admits a climb of up to
                    // STEP_H=20 per cell, but the controller's real step-up is movement::STEP_UP=2.0
                    // and it has no other ascent primitive (#239). So a grid "route" containing a
                    // riser taller than that is one the walker can never actually traverse — the grid
                    // is reporting a path it cannot walk. Measure the biggest single rise.
                    let mut max_rise: f32 = 0.0;
                    let mut prev = start;
                    for w in p.iter() {
                        max_rise = max_rise.max(w[2] - prev[2]);
                        prev = *w;
                    }
                    g_only_rise.push(max_rise);
                }
                (false, None)    => none_ += 1,
            }
        }
    }

    Ok(ZoneReport {
        zone: zone.into(), tris, bake_ms, cache_bytes,
        columns: cols_seen, surfaces: mesh.surface_count(), swim: mesh.swim_surface_count(),
        stacked_pct: if cols_seen > 0 { 100.0 * stacked as f32 / cols_seen as f32 } else { 0.0 },
        oracle_cov, player_only,
        pairs: both + m_only + g_only + none_,
        both_ok: both, mesh_only: m_only, grid_only: g_only, neither: none_,
        mesh_us_med: median(&mut m_us.clone()), grid_us_med: median(&mut g_us.clone()),
        mesh_us_max: m_us.iter().copied().max().unwrap_or(0),
        grid_us_max: g_us.iter().copied().max().unwrap_or(0),
        g_only_rise,
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
        let ceiling_is_walkable = mesh.column(c, r).iter().any(|s| (s.z - (-55.97)).abs() < 1.5);
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

fn main() -> Result<()> {
    let models = dirs::data_dir().unwrap().join("eqoxide/assets/models");
    // The oracle lives in the EQEmu server volume. Offline only — never read by the client.
    let nav_dir = std::path::PathBuf::from(
        "/home/dhenry/.local/share/containers/storage/volumes/eqemu_eqemu-server-data/_data/maps/nav");

    let mut args: Vec<String> = std::env::args().skip(1).collect();
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
    println!("oracle: EQEmu maps/nav (offline ground truth; NOT shipped, NOT loaded by the client)\n");

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

    println!("{:<13}{:>7}{:>8}{:>9}{:>8}{:>8}{:>7}{:>8} |{:>8}{:>9} |{:>6}{:>6}{:>6}{:>6} |{:>12}{:>12}",
        "zone", "tris", "bake_ms", "cache_kb", "columns", "surfs", "swim", "stack%",
        "oracle%", "plyonly%", "both", "mesh", "grid", "none", "mesh_us", "grid_us");
    println!("{}", "-".repeat(140));

    let mut reports = Vec::new();
    for z in &zones {
        match run_zone(z, &models, &nav_dir, n_pairs, params) {
            Ok(r) => {
                println!("{:<13}{:>7}{:>8}{:>9.0}{:>8}{:>8}{:>7}{:>7.1}% |{:>8}{:>9} |{:>6}{:>6}{:>6}{:>6} |{:>12}{:>12}",
                    r.zone, r.tris, r.bake_ms, r.cache_bytes as f32 / 1024.0, r.columns, r.surfaces,
                    r.swim, r.stacked_pct,
                    r.oracle_cov.map(|c| format!("{c:.1}")).unwrap_or_else(|| "-".into()),
                    r.player_only.map(|c| format!("{c:.1}")).unwrap_or_else(|| "-".into()),
                    r.both_ok, r.mesh_only, r.grid_only, r.neither,
                    format!("{}/{}", r.mesh_us_med, r.mesh_us_max),
                    format!("{}/{}", r.grid_us_med, r.grid_us_max));
                reports.push(r);
            }
            Err(e) => println!("{z:<13} ERROR: {e}"),
        }
    }

    if !reports.is_empty() {
        let n = reports.len();
        let bakes: Vec<u128> = reports.iter().map(|r| r.bake_ms).collect();
        let total_pairs: usize = reports.iter().map(|r| r.pairs).sum();
        let both: usize   = reports.iter().map(|r| r.both_ok).sum();
        let m_only: usize = reports.iter().map(|r| r.mesh_only).sum();
        let g_only: usize = reports.iter().map(|r| r.grid_only).sum();
        let none_: usize  = reports.iter().map(|r| r.neither).sum();
        let cache: usize  = reports.iter().map(|r| r.cache_bytes).sum();
        let covs: Vec<f32> = reports.iter().filter_map(|r| r.oracle_cov).collect();

        println!("\n=== TOTALS ({n} zones) ===");
        println!("BAKE TIME (the client's zone-load cost): median {} ms | max {} ms ({})",
            median(&mut bakes.clone()), bakes.iter().max().unwrap(),
            reports.iter().max_by_key(|r| r.bake_ms).unwrap().zone);
        println!("CACHE SIZE: total {:.1} MB over {n} zones | mean {:.0} KB/zone | max {:.0} KB ({})",
            cache as f32 / 1048576.0, cache as f32 / 1024.0 / n as f32,
            reports.iter().map(|r| r.cache_bytes).max().unwrap() as f32 / 1024.0,
            reports.iter().max_by_key(|r| r.cache_bytes).unwrap().zone);
        let mut all_m: Vec<u128> = reports.iter().map(|r| r.mesh_us_med).collect();
        let mut all_g: Vec<u128> = reports.iter().map(|r| r.grid_us_med).collect();
        println!("QUERY TIME: navmesh median {} us (worst {} us) | legacy grid median {} us (worst {} us)",
            median(&mut all_m), reports.iter().map(|r| r.mesh_us_max).max().unwrap(),
            median(&mut all_g), reports.iter().map(|r| r.grid_us_max).max().unwrap());
        if !covs.is_empty() {
            let mean = covs.iter().sum::<f32>() / covs.len() as f32;
            let worst = covs.iter().cloned().fold(f32::MAX, f32::min);
            let worst_z = &reports.iter().filter(|r| r.oracle_cov.is_some())
                .min_by(|a, b| a.oracle_cov.unwrap().partial_cmp(&b.oracle_cov.unwrap()).unwrap())
                .unwrap().zone;
            println!("ORACLE COVERAGE (our bake vs EQEmu's own navmesh): mean {mean:.1}% | worst {worst:.1}% ({worst_z})");
        }

        println!("\n=== GO / NO-GO ===");
        println!("A/B pairs: {total_pairs}   both={both}  mesh-only={m_only}  grid-only={g_only}  neither={none_}");
        let parity = 100.0 * (both + m_only + none_) as f32 / total_pairs.max(1) as f32;
        println!("parity-or-better reachability: {parity:.2}%   (gate: >= 95%)  ->  {}",
            if parity >= 95.0 { "PROCEED" } else { "DO NOT PROCEED — fall back to patching the grid" });
        println!("  routes the navmesh found that the grid could NOT: {m_only} ({:.1}%)",
            100.0 * m_only as f32 / total_pairs.max(1) as f32);
        println!("  routes the grid found that the navmesh could NOT: {g_only} ({:.1}%)",
            100.0 * g_only as f32 / total_pairs.max(1) as f32);

        // Are those grid-only routes REAL? The legacy A* climbs up to STEP_H=20 per cell, but the
        // controller can only step movement::STEP_UP=2.0 (#239). A grid route with a bigger riser is
        // one the walker physically cannot follow — the grid is reporting a path it cannot walk, so
        // counting it against the navmesh measures the grid's bug, not ours.
        let rises: Vec<f32> = reports.iter().flat_map(|r| r.g_only_rise.clone()).collect();
        if !rises.is_empty() {
            let unwalkable = rises.iter().filter(|&&r| r > eqoxide::movement::STEP_UP).count();
            let mut sorted = rises.clone();
            sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!("\n  of those {} grid-only routes, {} ({:.0}%) contain a single rise > STEP_UP({}u)",
                rises.len(), unwalkable, 100.0 * unwalkable as f32 / rises.len() as f32,
                eqoxide::movement::STEP_UP);
            println!("  -> the walker could NOT have followed them; the grid reported a path it cannot walk.");
            println!("  grid-only max-rise: median {:.1}u, p90 {:.1}u, max {:.1}u",
                sorted[sorted.len() / 2], sorted[sorted.len() * 9 / 10], sorted[sorted.len() - 1]);
            let genuine = rises.len() - unwalkable;
            let adj = 100.0 * (both + m_only + none_ + unwalkable) as f32 / total_pairs.max(1) as f32;
            println!("  GENUINE navmesh misses (grid route the walker really could walk): {genuine} ({:.1}%)",
                100.0 * genuine as f32 / total_pairs.max(1) as f32);
            println!("\n  --- ROOT CAUSE of each genuine miss ---");
            for m in reports.iter().flat_map(|r| r.misses.iter()) { println!("    {m}"); }
            println!("  parity discounting unwalkable grid routes: {adj:.2}%  ->  {}",
                if adj >= 95.0 { "PROCEED" } else { "still under the gate" });
        }
    }

    failure_cases(&models, params);
    Ok(())
}
