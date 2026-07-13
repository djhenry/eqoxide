//! Precomputed walkable-surface navmesh, baked CLIENT-SIDE from the zone's own collision mesh.
//!
//! # Why this exists
//!
//! The legacy nav graph (`assets::Collision::find_path`) has no *stored* notion of what a walkable
//! surface IS. It re-derives one per A* expansion by ray-casting the raw triangle soup
//! (`column_floors`, `nearest_floor`), with no surface-normal test and no clearance test. That is
//! the root of a whole bug family:
//!   * a CEILING counts as a floor (#329 — qcat anchors the route to the rock above the corridor),
//!   * the only thing stopping A* routing up into solid rock is an accidental 1.238-vs-1.2 grade
//!     margin (`assets.rs` MAX_WALK_GRADE, ascent-only, #313),
//!   * per-node cost is triangle-raycast-bound, so search time is geometry-dependent and unbounded
//!     — hence the 150 ms net-thread budget and the linkdead family (#257/#302/#340),
//!   * water has no surface layer at all, so swimmers path along the pool BOTTOM (#197 part 2).
//!
//! This module fixes the *representation*, not the symptoms. It runs the first three stages of the
//! standard Recast pipeline ONCE per zone, then caches the result to disk:
//!
//!   1. **Voxel heightfield** — rasterize every collision triangle into `cs`-sized XY columns,
//!      recording each solid span `[zmin, zmax]` and whether the triangle it came from is walkable
//!      by SLOPE (this is the normal filter `nearest_floor` never had).
//!   2. **Surface extraction** — a span's top is a walkable surface only if it is slope-walkable AND
//!      has `agent_height` of open air above it (this is the clearance filter that makes a ceiling
//!      stop being a floor, structurally — #329).
//!   3. **Water surface layer** (ours, not Recast's) — a swimmable body gets an explicit surface
//!      node at the waterline, so A* can cross a pool AT THE TOP instead of diving (#197 part 2).
//!   4. **Edge marking** — surfaces within `agent_radius` of a wall/ledge/waterline are FLAGGED, and
//!      A* pays to cross one. (Recast's hard erosion DELETES them; measured on the real zones that
//!      is far more aggressive than EQEmu's bake and disconnects narrow stairs and bridges.)
//!   5. **Connected components** — labelled once, over exactly the edges A* can traverse, with
//!      one-way FALL links kept directional. This makes "unreachable" an O(1) answer instead of
//!      something discovered by exhausting the search — which is the very thing that stalls the
//!      network thread for seconds today (#257/#302/#340).
//!
//! # What the result is
//!
//! A sparse layered grid: per XY column, a short sorted list of walkable surfaces (a z + flags).
//! Several surfaces per column is the normal case — that is what makes stacked geometry (catacombs,
//! multi-storey buildings, 192/497 zones with >10% stacked columns) natively expressible.
//!
//! Links between surfaces are NOT stored: they are derived at query time from the neighbouring
//! column's surface list, which is an O(1) array read instead of a Möller–Trumbore ray cast. That is
//! the ~100× per-node win, and it means this type is a drop-in for the `column_floors` /
//! `nearest_floor` role the A* in `assets.rs` already uses.
//!
//! DELIBERATELY NOT USED: EQEmu's prebuilt `maps/nav/*.nav` files. Navigation is a client-side
//! computation. Those files are used ONLY by the offline validation harness as a ground-truth
//! oracle (`tools/src/navmesh_validate.rs`); they are never read by the client and never shipped.

use crate::region_map::RegionMap;

/// Bake parameters. These are reconciled with OUR controller (`movement.rs`), not with EQEmu's
/// server-side bake — a mesh whose edges the walker physically cannot traverse would be a new lie.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BakeParams {
    /// Horizontal voxel size (EQ units). 2.0 matches the fine nav tier the walker already steers on.
    pub cell_size: f32,
    /// Vertical voxel quantum.
    pub cell_height: f32,
    /// Max walkable slope as rise/run. Matches `assets.rs` MAX_WALK_GRADE (1.2, ~50°). Unlike the
    /// legacy grid this is applied to the SURFACE NORMAL at bake time, so it governs descents and
    /// ceilings too — not just ascents (#313).
    pub max_grade: f32,
    /// Open air required above a surface for a character to stand there. EQEmu bakes 7.0; our
    /// collision rays top out at chest ~4.0, so 6.0 is the honest height for our controller.
    pub agent_height: f32,
    /// Clearance kept from walls/ledges. `movement::PLAYER_RADIUS` = 1.0 (EQEmu bakes 0.8).
    pub agent_radius: f32,
    /// Max height a surface may sit above its neighbour and still be WALKED onto. This is the
    /// native `movement::STEP_UP` (2.0) — NOT EQEmu's walkableClimb of 6.0, which would author
    /// edges our walker cannot climb.
    pub max_climb: f32,
}

impl Default for BakeParams {
    fn default() -> Self {
        Self {
            cell_size:    2.0,
            cell_height:  0.5,
            max_grade:    1.2,
            agent_height: 6.0,
            agent_radius: crate::movement::PLAYER_RADIUS,
            max_climb:    crate::movement::STEP_UP,
        }
    }
}

impl BakeParams {
    /// Pick a cell size for a zone of the given XY extent, so a huge outdoor zone does not explode
    /// into millions of cells.
    ///
    /// At a fixed 2u cell, gfaydark (a big outdoor zone) bakes 5.9M columns: 29 s to bake, a 33 MB
    /// cache and a 30 s worst-case query — all unacceptable. Recast solves this with its
    /// contour/polygon stage, which collapses cells into a few thousand convex polys; we do not have
    /// that stage, so we scale the cell instead. Indoor/dungeon zones (the ones with tight corridors
    /// and stacked floors, where resolution actually matters) stay at the fine 2u cell; only sprawling
    /// outdoor zones — which are mostly open ground — coarsen.
    pub fn for_extent(&self, extent_e: f32, extent_n: f32) -> BakeParams {
        // Measured: A* cost scales with the node count, and a graph much past ~300k columns pushes
        // the worst-case whole-zone query into seconds (gfaydark at a 2u cell: 635k columns, 4.4 s
        // worst). 300k keeps every zone's worst case in the tens of milliseconds.
        const TARGET_COLUMNS: f32 = 300_000.0;
        let area = (extent_e.max(1.0) * extent_n.max(1.0)).max(1.0);
        let needed = (area / TARGET_COLUMNS).sqrt();
        let cell = self.cell_size.max(needed).min(8.0);
        // Snap to a whole unit so the cache key (and the resulting mesh) is stable across runs.
        BakeParams { cell_size: cell.ceil(), ..*self }
    }

    /// Stable byte encoding, hashed into the cache key so a retuned parameter forces a re-bake.
    fn key_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        for f in [self.cell_size, self.cell_height, self.max_grade,
                  self.agent_height, self.agent_radius, self.max_climb] {
            v.extend_from_slice(&f.to_le_bytes());
        }
        v
    }
}

pub const FLAG_WALK: u8  = 1 << 0;
/// A swimmable water surface (the waterline), not solid ground.
pub const FLAG_SWIM: u8  = 1 << 1;
/// Within `agent_radius` of a wall, a ledge lip or a waterline.
///
/// Recast would DELETE these (hard erosion). We do not: at our cell size that deletes narrow stairs
/// and bridges outright and disconnects them (measured: it cost 15.5% of routes the legacy grid can
/// still find). Instead the surface survives and A* pays a penalty to use it, so a route keeps to
/// the middle of a corridor when it can and still threads a 4u bridge when it must.
pub const FLAG_EDGE: u8  = 1 << 2;

/// One walkable surface in a column: the height you stand (or float) at, plus what kind it is.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub z:     f32,
    pub flags: u8,
}

impl Surface {
    pub fn is_swim(&self) -> bool { self.flags & FLAG_SWIM != 0 }
}

/// A solid span during rasterization (stage 1 only).
#[derive(Clone, Copy)]
struct RawSpan {
    key:      u64,
    zmin:     f32,
    zmax:     f32,
    walkable: bool,
}

/// The baked navmesh: a sparse CSR of XY columns → sorted walkable surfaces.
pub struct NavMesh {
    pub params: BakeParams,
    origin:     [f32; 2],
    /// Sorted column keys (`col << 32 | row`), binary-searched on lookup.
    keys:       Vec<u64>,
    /// `offsets[i] .. offsets[i+1]` indexes `surfaces` for `keys[i]`.
    offsets:    Vec<u32>,
    /// Surfaces, ascending z within a column.
    surfaces:   Vec<Surface>,
    /// Component id per surface, over the BIDIRECTIONAL (walk/swim) edges only.
    comp:       Vec<u32>,
    /// Directed component→component edges contributed by one-way FALL links.
    comp_edges: Vec<Vec<u32>>,
    components: u32,
    /// blake3 of (source collision geometry + params) — the cache-invalidation key.
    pub digest: [u8; 32],
}

/// Can a character move BETWEEN these two surfaces in both directions? This is the honest
/// definition — it is what the controller can actually do (`movement::STEP_UP`), so a drop bigger
/// than a step is NOT traversable here; it is a one-way fall, handled separately.
#[inline]
fn traversable(a: Surface, b: Surface, max_climb: f32) -> bool {
    (b.z - a.z).abs() <= max_climb
}

#[inline]
fn ckey(c: i32, r: i32) -> u64 { ((c as u32 as u64) << 32) | (r as u32 as u64) }

impl NavMesh {
    pub fn column_count(&self)  -> usize { self.keys.len() }
    pub fn surface_count(&self) -> usize { self.surfaces.len() }
    pub fn origin(&self) -> [f32; 2] { self.origin }

    #[inline]
    pub fn to_cell(&self, east: f32, north: f32) -> (i32, i32) {
        (((east  - self.origin[0]) / self.params.cell_size).floor() as i32,
         ((north - self.origin[1]) / self.params.cell_size).floor() as i32)
    }

    #[inline]
    pub fn center(&self, c: i32, r: i32) -> [f32; 2] {
        [self.origin[0] + (c as f32 + 0.5) * self.params.cell_size,
         self.origin[1] + (r as f32 + 0.5) * self.params.cell_size]
    }

    /// Every populated `(col, row)`, for diagnostics and the offline validation harness.
    pub fn populated_columns(&self) -> impl Iterator<Item = (i32, i32)> + '_ {
        self.keys.iter().map(|&k| ((k >> 32) as u32 as i32, (k & 0xffff_ffff) as u32 as i32))
    }

    /// How many surfaces are swimmable waterlines (the #197p2 layer).
    pub fn swim_surface_count(&self) -> usize {
        self.surfaces.iter().filter(|s| s.is_swim()).count()
    }

    /// Every walkable surface in a column, ascending z. O(log n) — an array read, NOT a raycast.
    /// This is the drop-in replacement for `Collision::column_floors`.
    #[inline]
    pub fn column(&self, c: i32, r: i32) -> &[Surface] {
        match self.keys.binary_search(&ckey(c, r)) {
            Ok(i) => &self.surfaces[self.offsets[i] as usize..self.offsets[i + 1] as usize],
            Err(_) => &[],
        }
    }

    /// The walkable surface at `(east, north)` nearest `ref_z`. Drop-in for `nearest_floor` — but a
    /// ceiling can never be returned, because a ceiling is not a surface in this representation.
    pub fn nearest_surface(&self, east: f32, north: f32, ref_z: f32) -> Option<Surface> {
        let (c, r) = self.to_cell(east, north);
        self.column(c, r).iter()
            .min_by(|a, b| (a.z - ref_z).abs().partial_cmp(&(b.z - ref_z).abs()).unwrap())
            .copied()
    }

    // ───────────────────────────── bake ─────────────────────────────

    /// Bake from world-space collision triangles `[[east, north, z]; 3]`, optionally layering a
    /// water surface from the zone's `.wtr` region map. `digest_src` is hashed with the params into
    /// the cache key (pass the source GLB bytes).
    pub fn bake(tris: &[[[f32; 3]; 3]], water: Option<&RegionMap>, params: BakeParams,
                digest_src: &[u8]) -> NavMesh {
        let mut hasher = blake3::Hasher::new();
        hasher.update(digest_src);
        hasher.update(&params.key_bytes());
        let digest: [u8; 32] = hasher.finalize().into();

        if tris.is_empty() {
            return NavMesh { params, origin: [0.0, 0.0], keys: vec![], offsets: vec![0],
                             surfaces: vec![], comp: vec![], comp_edges: vec![], components: 0, digest };
        }

        // XY origin + extent (the extent picks the cell size — see BakeParams::for_extent).
        let (mut min_e, mut min_n) = (f32::MAX, f32::MAX);
        let (mut max_e, mut max_n) = (f32::MIN, f32::MIN);
        for t in tris {
            for v in t {
                min_e = min_e.min(v[0]); min_n = min_n.min(v[1]);
                max_e = max_e.max(v[0]); max_n = max_n.max(v[1]);
            }
        }
        let origin = [min_e, min_n];
        let params = params.for_extent(max_e - min_e, max_n - min_n);
        let cs = params.cell_size;

        // ── Stage 1: voxel heightfield. Clip each triangle to each column it overlaps and record
        // the solid span there. `nz` (the surface normal's up component) decides slope-walkability —
        // the filter `nearest_floor` never had. A downward-facing face (a ceiling) has nz < 0 and can
        // never be walkable, so #329's ceiling-as-floor is impossible by construction.
        let cos_max = 1.0 / (1.0 + params.max_grade * params.max_grade).sqrt();
        let mut raw: Vec<RawSpan> = Vec::new();
        for t in tris {
            let e1 = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
            let e2 = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
            let n  = [e1[1] * e2[2] - e1[2] * e2[1],
                      e1[2] * e2[0] - e1[0] * e2[2],
                      e1[0] * e2[1] - e1[1] * e2[0]];
            let nl = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if nl < 1e-9 { continue; }
            // SIGNED nz — this is the whole ballgame, and it is the filter `nearest_floor` lacks.
            //
            // EQ zone art is a THIN SHELL, not a solid: a cave ceiling is a single polygon with open
            // air above it. So a clearance test ALONE cannot tell a ceiling from a floor (the qcat
            // ceiling has 14u of air above it and would pass). The only thing that separates them is
            // which way the face points, and winding IS reliable here — measured across the shipped
            // collision meshes, down-facing faces sit at a consistently HIGHER median z than
            // up-facing ones in every zone (qcat: up −42.0 vs down −28.0; halas: 3.0 vs 599.8).
            //
            // So: only an UP-facing face within the walkable grade can ever be stood on. A ceiling
            // (nz < 0) is not a surface at any z, which is #329 fixed by construction rather than by
            // a tolerance. This also governs descents, unlike the legacy ascent-only grade cap (#313).
            let nz = n[2] / nl;
            let walkable = nz >= cos_max;

            let (mut tmin_e, mut tmax_e) = (f32::MAX, f32::MIN);
            let (mut tmin_n, mut tmax_n) = (f32::MAX, f32::MIN);
            for v in t {
                tmin_e = tmin_e.min(v[0]); tmax_e = tmax_e.max(v[0]);
                tmin_n = tmin_n.min(v[1]); tmax_n = tmax_n.max(v[1]);
            }
            let c0 = ((tmin_e - origin[0]) / cs).floor() as i32;
            let c1 = ((tmax_e - origin[0]) / cs).floor() as i32;
            let r0 = ((tmin_n - origin[1]) / cs).floor() as i32;
            let r1 = ((tmax_n - origin[1]) / cs).floor() as i32;
            for r in r0..=r1 {
                for c in c0..=c1 {
                    let x0 = origin[0] + c as f32 * cs;
                    let y0 = origin[1] + r as f32 * cs;
                    // Clip the triangle to this column's cell square (Sutherland–Hodgman, 4 planes);
                    // the clipped polygon's z-range is the solid span this triangle contributes.
                    let poly = [ [t[0][0], t[0][1], t[0][2]],
                                 [t[1][0], t[1][1], t[1][2]],
                                 [t[2][0], t[2][1], t[2][2]] ];
                    let Some((zmin, zmax)) = clip_z_range(&poly, x0, y0, x0 + cs, y0 + cs) else { continue };
                    raw.push(RawSpan { key: ckey(c, r), zmin, zmax, walkable });
                }
            }
        }
        if raw.is_empty() {
            return NavMesh { params, origin, keys: vec![], offsets: vec![0], surfaces: vec![],
                             comp: vec![], comp_edges: vec![], components: 0, digest };
        }

        // Group spans by column, merging overlapping solids (a floor made of many triangles is one
        // span). Sorting by (column, zmin) makes both passes linear.
        raw.sort_unstable_by(|a, b| a.key.cmp(&b.key)
            .then(a.zmin.partial_cmp(&b.zmin).unwrap_or(std::cmp::Ordering::Equal)));

        let mut keys: Vec<u64> = Vec::new();
        let mut offsets: Vec<u32> = vec![0];
        let mut surfaces: Vec<Surface> = Vec::new();
        // Column-local scratch.
        let mut merged: Vec<RawSpan> = Vec::new();

        let mut i = 0usize;
        while i < raw.len() {
            let key = raw[i].key;
            let mut j = i;
            merged.clear();
            while j < raw.len() && raw[j].key == key {
                let s = raw[j];
                match merged.last_mut() {
                    // Overlapping / touching solids merge into one span. A walkable top wins over a
                    // non-walkable one only if it is at (or above) the merged top — a walkable floor
                    // laid on a steep rock still walks.
                    Some(m) if s.zmin <= m.zmax + params.cell_height => {
                        if s.zmax >= m.zmax { m.walkable = s.walkable; m.zmax = m.zmax.max(s.zmax); }
                    }
                    _ => merged.push(s),
                }
                j += 1;
            }

            // ── Stage 2: a span's TOP is a walkable surface iff it is slope-walkable AND has
            // `agent_height` of open air above it. This is what structurally kills #329: qcat's
            // ceiling at −55.97 has the rock directly above it (no clearance), so it is not a
            // surface, and A* cannot anchor to it no matter what z the caller reports.
            let start = surfaces.len();
            for (k, m) in merged.iter().enumerate() {
                if !m.walkable { continue; }
                let ceil = merged.get(k + 1).map(|n| n.zmin).unwrap_or(f32::INFINITY);
                if ceil - m.zmax < params.agent_height { continue; }
                surfaces.push(Surface { z: m.zmax, flags: FLAG_WALK });
            }
            if surfaces.len() > start {
                keys.push(key);
                offsets.push(surfaces.len() as u32);
            }
            i = j;
        }

        let mut mesh = NavMesh { params, origin, keys, offsets, surfaces,
                                 comp: vec![], comp_edges: vec![], components: 0, digest };
        if let Some(w) = water { mesh.add_water_layer(w); }
        mesh.mark_edges();
        mesh.label_components();
        mesh.link_components();
        mesh
    }

    /// Directed component→component edges from one-way FALL links (you can drop off a ledge into a
    /// sealed pit; you cannot climb back). Keeps the O(1) reachability test HONEST: a goal you can
    /// only fall into is reachable; a goal you could only reach by climbing back out is not.
    fn link_components(&mut self) {
        let mut edges: Vec<std::collections::HashSet<u32>> =
            vec![Default::default(); self.components as usize];
        for si in 0..self.surfaces.len() {
            let (c, r, s) = self.locate(si);
            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (nc, nr) = (c + dc, r + dr);
                let base = match self.keys.binary_search(&ckey(nc, nr)) {
                    Ok(i) => self.offsets[i] as usize,
                    Err(_) => continue,
                };
                for (k, ns) in self.column(nc, nr).iter().enumerate() {
                    let drop = s.z - ns.z;
                    if drop > self.params.max_climb && drop <= MAX_FALL {
                        let (a, b) = (self.comp[si], self.comp[base + k]);
                        if a != b { edges[a as usize].insert(b); }
                    }
                }
            }
        }
        self.comp_edges = edges.into_iter().map(|s| s.into_iter().collect()).collect();
    }

    /// Is `gi` reachable from `si` at all? An O(components) test, answered BEFORE any A* expansion.
    fn comp_reachable(&self, si: usize, gi: usize) -> bool {
        let (a, b) = (self.comp[si], self.comp[gi]);
        if a == b { return true; }
        let mut seen = vec![false; self.components as usize];
        let mut stack = vec![a];
        seen[a as usize] = true;
        while let Some(c) = stack.pop() {
            if c == b { return true; }
            for &n in &self.comp_edges[c as usize] {
                if !seen[n as usize] { seen[n as usize] = true; stack.push(n); }
            }
        }
        false
    }

    /// ── Stage 3: mark (do NOT delete) surfaces within `agent_radius` of an edge.
    ///
    /// A surface whose 4-neighbour column has no surface within `max_climb` is an EDGE: a wall foot,
    /// a ledge lip, a waterline. Recast's erosion DELETES everything within the agent radius of one.
    /// Measured on the real zones, hard erosion at our cell size is far more aggressive than EQEmu's
    /// (~2.0u vs ~0.8u) and it deletes narrow stairs and bridges entirely — it cost 15.5% of the
    /// routes the legacy grid can still find, which would be a regression, not a fix.
    ///
    /// So we mark instead. `FLAG_EDGE` surfaces stay in the graph (connectivity preserved) and A*
    /// pays `EDGE_PENALTY` to cross one, which keeps routes off walls where there is room and still
    /// lets them thread a 4u bridge where there is not.
    fn mark_edges(&mut self) {
        let rad_cells = (self.params.agent_radius / self.params.cell_size).ceil().max(1.0) as i32;
        let mut dist: Vec<i32> = vec![i32::MAX; self.surfaces.len()];
        let mut queue: std::collections::VecDeque<usize> = std::collections::VecDeque::new();

        for si in 0..self.surfaces.len() {
            let (c, r, s) = self.locate(si);
            let mut sides = 0;
            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                if self.column(c + dc, r + dr).iter()
                    .any(|ns| (ns.z - s.z).abs() <= self.params.max_climb) { sides += 1; }
            }
            if sides < 4 { dist[si] = 0; queue.push_back(si); }
        }
        while let Some(si) = queue.pop_front() {
            let d = dist[si];
            if d >= rad_cells { continue; }
            let (c, r, s) = self.locate(si);
            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1)] {
                let (nc, nr) = (c + dc, r + dr);
                let base = match self.keys.binary_search(&ckey(nc, nr)) {
                    Ok(i) => self.offsets[i] as usize,
                    Err(_) => continue,
                };
                for (k, ns) in self.column(nc, nr).iter().enumerate() {
                    if (ns.z - s.z).abs() > self.params.max_climb { continue; }
                    let ni = base + k;
                    if dist[ni] > d + 1 { dist[ni] = d + 1; queue.push_back(ni); }
                }
            }
        }
        for (si, s) in self.surfaces.iter_mut().enumerate() {
            if dist[si] < rad_cells { s.flags |= FLAG_EDGE; }
        }
    }

    /// ── Stage 5: connected components over exactly the edges A* can traverse.
    ///
    /// This is what makes "unreachable" an HONEST, INSTANT answer. The legacy planner discovers
    /// unreachability by exhausting the search — which is precisely the pathological case that
    /// blocks the network thread for seconds and trips the linkdead guard (#257/#302/#340). With a
    /// component id per surface, `find_path` rejects an impossible goal with one integer compare and
    /// never expands a node.
    fn label_components(&mut self) {
        let n = self.surfaces.len();
        self.comp = vec![u32::MAX; n];
        let mut next = 0u32;
        let mut stack: Vec<usize> = Vec::new();
        for seed in 0..n {
            if self.comp[seed] != u32::MAX { continue; }
            self.comp[seed] = next;
            stack.push(seed);
            while let Some(si) = stack.pop() {
                let (c, r, s) = self.locate(si);
                let mut found: Vec<usize> = Vec::new();
                for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                    let (nc, nr) = (c + dc, r + dr);
                    let base = match self.keys.binary_search(&ckey(nc, nr)) {
                        Ok(i) => self.offsets[i] as usize,
                        Err(_) => continue,
                    };
                    for (k, ns) in self.column(nc, nr).iter().enumerate() {
                        let ni = base + k;
                        if self.comp[ni] != u32::MAX { continue; }
                        if !traversable(s, *ns, self.params.max_climb) { continue; }
                        found.push(ni);
                    }
                }
                for ni in found {
                    if self.comp[ni] != u32::MAX { continue; }
                    self.comp[ni] = next;
                    stack.push(ni);
                }
            }
            next += 1;
        }
        self.components = next;
    }

    /// ── Stage 4 (ours): the water-surface layer (#197 part 2).
    ///
    /// The legacy grid has NO representation of a waterline, so A* plans a swimmer along the pool
    /// BOTTOM and the walker dives and strands; no budget or anchor fix can create a layer that the
    /// model cannot express. Here it is just another surface in the column, flagged `FLAG_SWIM`, so
    /// crossing a pool at the top is an ordinary A* edge and diving is a separate (costlier) one.
    fn add_water_layer(&mut self, water: &RegionMap) {
        let mut add: Vec<(u64, Surface)> = Vec::new();
        for i in 0..self.keys.len() {
            let (c, r) = ((self.keys[i] >> 32) as u32 as i32, (self.keys[i] & 0xffff_ffff) as u32 as i32);
            let p = self.center(c, r);
            let mut seen: Vec<f32> = Vec::new();
            for s in self.column(c, r) {
                // Is this solid surface submerged? Probe just above it (a floor exactly AT the
                // waterline reads dry).
                if !water.is_water(p[0], p[1], s.z + 2.0) { continue; }
                let Some(surf) = water.surface_z(p[0], p[1], s.z + 2.0) else { continue };
                // Only a genuine layer ABOVE the floor: a puddle you can wade is not a swim surface.
                if surf - s.z < self.params.agent_height { continue; }
                if seen.iter().any(|&z| (z - surf).abs() < 1.0) { continue; }
                seen.push(surf);
                add.push((ckey(c, r), Surface { z: surf, flags: FLAG_SWIM }));
            }
        }
        if add.is_empty() { return; }
        // Merge into the CSR, keeping each column ascending in z.
        let mut by_key: std::collections::HashMap<u64, Vec<Surface>> = std::collections::HashMap::new();
        for (k, s) in add { by_key.entry(k).or_default().push(s); }

        let mut keys = Vec::with_capacity(self.keys.len());
        let mut offsets = vec![0u32];
        let mut surfaces = Vec::with_capacity(self.surfaces.len());
        for i in 0..self.keys.len() {
            let k = self.keys[i];
            let mut col: Vec<Surface> =
                self.surfaces[self.offsets[i] as usize..self.offsets[i + 1] as usize].to_vec();
            if let Some(extra) = by_key.get(&k) { col.extend_from_slice(extra); }
            col.sort_by(|a, b| a.z.partial_cmp(&b.z).unwrap_or(std::cmp::Ordering::Equal));
            surfaces.extend_from_slice(&col);
            keys.push(k);
            offsets.push(surfaces.len() as u32);
        }
        self.keys = keys;
        self.offsets = offsets;
        self.surfaces = surfaces;
    }

    /// (col, row, surface) for a flat surface index.
    fn locate(&self, si: usize) -> (i32, i32, Surface) {
        let i = match self.offsets.binary_search(&(si as u32)) {
            Ok(mut i) => { while self.offsets[i + 1] as usize <= si { i += 1; } i }
            Err(i) => i - 1,
        };
        let k = self.keys[i];
        ((k >> 32) as u32 as i32, (k & 0xffff_ffff) as u32 as i32, self.surfaces[si])
    }


    // ───────────────────────────── query ─────────────────────────────

    /// A* over the surface graph. Returns `[east, north, z]` waypoints (start-exclusive,
    /// goal-inclusive), or `None` if the goal is unreachable.
    ///
    /// Every neighbour lookup is an array read, so there is no per-node raycast, no
    /// geometry-dependent blowup, and no need for a wall-clock budget.
    pub fn find_path(&self, start: [f32; 3], goal: [f32; 3]) -> Option<Vec<[f32; 3]>> {
        use std::collections::BinaryHeap;
        use std::cmp::Ordering;
        if self.keys.is_empty() { return None; }

        let sidx = self.nearest_index(start)?;
        let gidx = self.nearest_index(goal)?;
        // HONEST + INSTANT unreachability. The legacy planner can only discover this by exhausting
        // the search, which is exactly the pathological case that stalls the network thread for
        // seconds (#257/#302/#340). Here it costs one component lookup and zero expansions.
        if !self.comp_reachable(sidx, gidx) { return None; }
        if sidx == gidx {
            let (c, r, s) = self.locate(gidx);
            let p = self.center(c, r);
            return Some(vec![[p[0], p[1], s.z]]);
        }
        let (gc, gr, _) = self.locate(gidx);
        let cs = self.params.cell_size;
        let h = |c: i32, r: i32| (((c - gc) as f32).powi(2) + ((r - gr) as f32).powi(2)).sqrt() * cs;

        struct Node { f: f32, si: usize, c: i32, r: i32 }
        impl PartialEq for Node { fn eq(&self, o: &Self) -> bool { self.f == o.f } }
        impl Eq for Node {}
        impl Ord for Node {
            fn cmp(&self, o: &Self) -> Ordering { o.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal) }
        }
        impl PartialOrd for Node { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) } }

        // Flat arrays, not hash maps: on a big outdoor zone this graph has millions of nodes and
        // hashing dominated the search (measured: gfaydark went from a 30 s worst case to ~1 s).
        let mut g: Vec<f32> = vec![f32::MAX; self.surfaces.len()];
        let mut came: Vec<u32> = vec![u32::MAX; self.surfaces.len()];
        let mut closed = vec![false; self.surfaces.len()];
        let (sc, sr, _) = self.locate(sidx);
        g[sidx] = 0.0;
        let mut heap = BinaryHeap::new();
        heap.push(Node { f: h(sc, sr), si: sidx, c: sc, r: sr });

        while let Some(Node { si, c, r, .. }) = heap.pop() {
            if si == gidx { break; }
            if closed[si] { continue; }
            closed[si] = true;
            let s = self.surfaces[si];
            let g_cur = g[si];

            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (nc, nr) = (c + dc, r + dr);
                let base = match self.keys.binary_search(&ckey(nc, nr)) {
                    Ok(i) => self.offsets[i] as usize,
                    Err(_) => continue,
                };
                let run = ((dc * dc + dr * dr) as f32).sqrt() * cs;
                for (k, ns) in self.column(nc, nr).iter().enumerate() {
                    let ni = base + k;
                    if closed[ni] { continue; }
                    let rise = ns.z - s.z;

                    // Edge admissibility, straight from the controller's real capabilities.
                    let mut cost = if s.is_swim() && ns.is_swim() {
                        // Swim across the surface — the #197p2 edge the legacy model cannot express.
                        if rise.abs() > self.params.max_climb { continue; }
                        run * 1.5
                    } else if s.is_swim() != ns.is_swim() {
                        // Enter/leave the water: only at a low lip (the controller's swim step-up).
                        if rise.abs() > self.params.max_climb { continue; }
                        run * 2.0
                    } else if rise > self.params.max_climb {
                        continue; // taller than the native STEP_UP: a wall, not a step.
                    } else if rise < -MAX_FALL {
                        continue; // lethal / unrecoverable drop.
                    } else if rise < -self.params.max_climb {
                        // A drop the walker must fall down. Costly: A* takes stairs when they exist.
                        run + (-rise) * 2.0 + FALL_PENALTY
                    } else {
                        run + rise.abs() * 0.5
                    };
                    // Prefer the middle of a corridor over its wall/ledge/waterline lip — but never
                    // refuse the lip, or a narrow bridge becomes unroutable (see `mark_edges`).
                    if ns.flags & FLAG_EDGE != 0 { cost += EDGE_PENALTY; }

                    let tentative = g_cur + cost;
                    if tentative < g[ni] {
                        g[ni] = tentative;
                        came[ni] = si as u32;
                        heap.push(Node { f: tentative + h(nc, nr), si: ni, c: nc, r: nr });
                    }
                }
            }
        }
        if came[gidx] == u32::MAX { return None; }

        let mut path = Vec::new();
        let mut cur = gidx;
        while cur != sidx {
            let (c, r, s) = self.locate(cur);
            let p = self.center(c, r);
            path.push([p[0], p[1], s.z]);
            if came[cur] == u32::MAX { break; }
            cur = came[cur] as usize;
        }
        path.reverse();
        Some(self.string_pull(start, path))
    }

    /// Drop waypoints that a straight line already covers. Line-of-sight is tested over the SURFACE
    /// GRID (every column crossed must hold a surface at the interpolated height) — no triangle
    /// raycast, so smoothing stays as cheap as the search.
    fn string_pull(&self, start: [f32; 3], path: Vec<[f32; 3]>) -> Vec<[f32; 3]> {
        if path.len() < 3 { return path; }
        let mut out: Vec<[f32; 3]> = Vec::new();
        let mut anchor = start;
        let mut i = 0usize;
        while i < path.len() {
            let mut j = path.len() - 1;
            while j > i {
                if self.los(anchor, path[j]) { break; }
                j -= 1;
            }
            out.push(path[j]);
            anchor = path[j];
            i = j + 1;
        }
        out
    }

    fn los(&self, a: [f32; 3], b: [f32; 3]) -> bool {
        let cs = self.params.cell_size;
        let d = ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2)).sqrt();
        let steps = (d / (cs * 0.5)).ceil() as i32;
        if steps <= 1 { return true; }
        for k in 1..steps {
            let t = k as f32 / steps as f32;
            let (x, y) = (a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t);
            let z = a[2] + (b[2] - a[2]) * t;
            let (c, r) = self.to_cell(x, y);
            if !self.column(c, r).iter().any(|s| (s.z - z).abs() <= self.params.max_climb) {
                return false;
            }
        }
        true
    }

    /// Nearest surface index to a world point, preferring the surface closest in z (so a caller's
    /// stale z cannot anchor it to the wrong tier of a stacked column — the #229/#344 failure).
    /// Searches outward a few cells so a point inside a wall or just off the mesh still anchors.
    pub fn nearest_index(&self, p: [f32; 3]) -> Option<usize> {
        let (c0, r0) = self.to_cell(p[0], p[1]);
        let mut best: Option<(f32, usize)> = None;
        for rad in 0..=6i32 {
            for dr in -rad..=rad {
                for dc in -rad..=rad {
                    if rad > 0 && dc.abs() != rad && dr.abs() != rad { continue; }
                    let (c, r) = (c0 + dc, r0 + dr);
                    let base = match self.keys.binary_search(&ckey(c, r)) {
                        Ok(i) => self.offsets[i] as usize,
                        Err(_) => continue,
                    };
                    let ctr = self.center(c, r);
                    for (k, s) in self.column(c, r).iter().enumerate() {
                        // Weight z-error heavily: on stacked geometry the right TIER matters far more
                        // than a couple of cells of XY error.
                        let d = (ctr[0] - p[0]).powi(2) + (ctr[1] - p[1]).powi(2)
                              + (s.z - p[2]).powi(2) * 4.0;
                        if best.map_or(true, |(bd, _)| d < bd) { best = Some((d, base + k)); }
                    }
                }
            }
            if best.is_some() { break; }
        }
        best.map(|(_, i)| i)
    }

    // ───────────────────────────── cache ─────────────────────────────

    /// Serialize (deflate). Format: magic, version, digest, params, CSR.
    pub fn serialize(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(b"EQOXNAVM");
        v.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        v.extend_from_slice(&self.digest);
        for f in [self.params.cell_size, self.params.cell_height, self.params.max_grade,
                  self.params.agent_height, self.params.agent_radius, self.params.max_climb,
                  self.origin[0], self.origin[1]] {
            v.extend_from_slice(&f.to_le_bytes());
        }
        v.extend_from_slice(&(self.keys.len() as u32).to_le_bytes());
        v.extend_from_slice(&(self.surfaces.len() as u32).to_le_bytes());
        for k in &self.keys { v.extend_from_slice(&k.to_le_bytes()); }
        for o in &self.offsets { v.extend_from_slice(&o.to_le_bytes()); }
        for s in &self.surfaces {
            v.extend_from_slice(&s.z.to_le_bytes());
            v.push(s.flags);
        }
        miniz_oxide::deflate::compress_to_vec(&v, 6)
    }

    /// Load a cached mesh, REJECTING it unless the digest matches `(digest_src, params)` exactly.
    /// A changed zone GLB or a retuned bake parameter therefore forces a re-bake automatically —
    /// silently pathing on a stale mesh would be exactly the kind of lie this module exists to end.
    pub fn deserialize(blob: &[u8], digest_src: &[u8], params: BakeParams) -> Option<NavMesh> {
        let v = miniz_oxide::inflate::decompress_to_vec(blob).ok()?;
        if v.len() < 8 + 4 + 32 || &v[..8] != b"EQOXNAVM" { return None; }
        if u32::from_le_bytes(v[8..12].try_into().ok()?) != CACHE_VERSION { return None; }
        let digest: [u8; 32] = v[12..44].try_into().ok()?;

        let mut hasher = blake3::Hasher::new();
        hasher.update(digest_src);
        hasher.update(&params.key_bytes());
        let want: [u8; 32] = hasher.finalize().into();
        if digest != want { return None; } // stale cache → caller re-bakes.

        let rf = |o: usize| f32::from_le_bytes(v[o..o + 4].try_into().unwrap());
        let mut o = 44;
        let params = BakeParams {
            cell_size: rf(o), cell_height: rf(o + 4), max_grade: rf(o + 8),
            agent_height: rf(o + 12), agent_radius: rf(o + 16), max_climb: rf(o + 20),
        };
        o += 24;
        let origin = [rf(o), rf(o + 4)];
        o += 8;
        let nk = u32::from_le_bytes(v[o..o + 4].try_into().ok()?) as usize; o += 4;
        let ns = u32::from_le_bytes(v[o..o + 4].try_into().ok()?) as usize; o += 4;
        if v.len() < o + nk * 8 + (nk + 1) * 4 + ns * 5 { return None; }

        let mut keys = Vec::with_capacity(nk);
        for i in 0..nk { keys.push(u64::from_le_bytes(v[o + i * 8..o + i * 8 + 8].try_into().ok()?)); }
        o += nk * 8;
        let mut offsets = Vec::with_capacity(nk + 1);
        for i in 0..=nk { offsets.push(u32::from_le_bytes(v[o + i * 4..o + i * 4 + 4].try_into().ok()?)); }
        o += (nk + 1) * 4;
        let mut surfaces = Vec::with_capacity(ns);
        for i in 0..ns {
            let b = o + i * 5;
            surfaces.push(Surface { z: f32::from_le_bytes(v[b..b + 4].try_into().ok()?), flags: v[b + 4] });
        }
        let mut mesh = NavMesh { params, origin, keys, offsets, surfaces,
                                 comp: vec![], comp_edges: vec![], components: 0, digest };
        // Components are derived, not stored: relabelling is a pure BFS over the surface graph
        // (no triangle work), so it is far cheaper than the bytes it would cost on disk.
        mesh.label_components();
        mesh.link_components();
        Some(mesh)
    }
}

const CACHE_VERSION: u32 = 2;
/// Max drop A* will plan (matches the legacy MAX_FALL).
const MAX_FALL: f32 = 120.0;
/// A fall is a last resort — take the stairs if they exist (matches the legacy FALL_PENALTY intent).
const FALL_PENALTY: f32 = 5_000.0;
/// Cost of routing over a surface within `agent_radius` of a wall/ledge/waterline. Big enough to
/// push a route to the middle of a corridor, small enough that a narrow bridge is still taken.
const EDGE_PENALTY: f32 = 6.0;

/// Clip a triangle to the cell square `[x0,x1] × [y0,y1]` and return the clipped polygon's z-range,
/// or `None` if the triangle does not actually cover the cell. This is Recast's rasterizer: an
/// AABB-overlap test alone would smear a long diagonal triangle across columns it never touches.
fn clip_z_range(tri: &[[f32; 3]; 3], x0: f32, y0: f32, x1: f32, y1: f32) -> Option<(f32, f32)> {
    let mut poly: Vec<[f32; 3]> = tri.to_vec();
    // 4 half-planes: x>=x0, x<=x1, y>=y0, y<=y1.
    for (axis, bound, keep_greater) in
        [(0usize, x0, true), (0, x1, false), (1, y0, true), (1, y1, false)]
    {
        if poly.is_empty() { return None; }
        let mut out: Vec<[f32; 3]> = Vec::with_capacity(poly.len() + 1);
        for i in 0..poly.len() {
            let a = poly[i];
            let b = poly[(i + 1) % poly.len()];
            let da = if keep_greater { a[axis] - bound } else { bound - a[axis] };
            let db = if keep_greater { b[axis] - bound } else { bound - b[axis] };
            let ain = da >= 0.0;
            let bin = db >= 0.0;
            if ain { out.push(a); }
            if ain != bin {
                let t = da / (da - db);
                out.push([a[0] + (b[0] - a[0]) * t,
                          a[1] + (b[1] - a[1]) * t,
                          a[2] + (b[2] - a[2]) * t]);
            }
        }
        poly = out;
    }
    if poly.is_empty() { return None; }
    let mut zmin = f32::MAX;
    let mut zmax = f32::MIN;
    for p in &poly { zmin = zmin.min(p[2]); zmax = zmax.max(p[2]); }
    Some((zmin, zmax))
}

/// Extract world-space collision triangles from loaded zone assets, exactly as `Collision::build`
/// does (preferring the baked `__collision__` mesh: SOLID + INVIS faces, PASSABLE excluded).
pub fn collision_tris(assets: &crate::assets::ZoneAssets) -> Vec<[[f32; 3]; 3]> {
    use crate::assets::COLLISION_MESH_TAG;
    let expanded = crate::assets::expand_objects(&assets.objects);
    let from_collision_mesh = assets.terrain.iter()
        .any(|m| m.texture_name.as_deref() == Some(COLLISION_MESH_TAG));
    let terrain: Vec<&crate::assets::MeshData> = if from_collision_mesh {
        assets.terrain.iter()
            .filter(|m| m.texture_name.as_deref() == Some(COLLISION_MESH_TAG)).collect()
    } else {
        assets.terrain.iter().collect()
    };
    let mut tris = Vec::new();
    for m in terrain.into_iter().chain(expanded.iter()) {
        let (pos, idx) = (&m.positions, &m.indices);
        let mut k = 0;
        while k + 2 < idx.len() {
            let (ia, ib, ic) = (idx[k] as usize, idx[k + 1] as usize, idx[k + 2] as usize);
            k += 3;
            if ia >= pos.len() || ib >= pos.len() || ic >= pos.len() { continue; }
            // EQ WLD → world: east = p[2], north = p[0], up = p[1] (matches Collision::build).
            tris.push([
                [pos[ia][2] + m.center[2], pos[ia][0] + m.center[0], pos[ia][1] + m.center[1]],
                [pos[ib][2] + m.center[2], pos[ib][0] + m.center[0], pos[ib][1] + m.center[1]],
                [pos[ic][2] + m.center[2], pos[ic][0] + m.center[0], pos[ic][1] + m.center[1]],
            ]);
        }
    }
    tris
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two triangles forming an UP-facing z-plane quad over [e0,e1] × [n0,n1] (a floor).
    fn quad(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> Vec<[[f32; 3]; 3]> {
        vec![
            [[e0, n0, z], [e1, n0, z], [e1, n1, z]],
            [[e0, n0, z], [e1, n1, z], [e0, n1, z]],
        ]
    }

    /// The same quad wound the other way: a DOWN-facing plane — i.e. a ceiling, as EQ actually
    /// bakes one (a thin shell with open air above it, not a solid).
    fn quad_down(z: f32, e0: f32, e1: f32, n0: f32, n1: f32) -> Vec<[[f32; 3]; 3]> {
        quad(z, e0, e1, n0, n1).into_iter()
            .map(|t| [t[0], t[2], t[1]])
            .collect()
    }

    #[test]
    fn a_ceiling_is_never_a_walkable_surface() {
        // THE #329 CASE, structurally. The qcat spawn corridor: an up-facing floor at -69.97 and,
        // 14u above it, the DOWN-facing rock ceiling at -55.97. The corridor is flooded and the
        // water surface (-56.00) is flush with that ceiling — so the client reports a player z of
        // ~-56, and the legacy `nearest_floor` (which gathers every triangle a vertical ray crosses,
        // with NO normal test) hands back the CEILING as the floor. A* then plans the entire route
        // across the ceiling plane and the character wedges.
        //
        // Note this is NOT fixed by a clearance test alone: EQ geometry is a thin shell, so there is
        // 14u of open air above this ceiling too. It is fixed by the face pointing DOWN.
        let mut tris = quad(-69.97, 0.0, 40.0, 0.0, 40.0);      // corridor floor  (up-facing)
        tris.extend(quad_down(-55.97, 0.0, 40.0, 0.0, 40.0));   // rock ceiling    (down-facing) ← impostor
        let mesh = NavMesh::bake(&tris, None, BakeParams::default(), b"t");

        let (c, r) = mesh.to_cell(20.0, 20.0);
        let col = mesh.column(c, r);
        // The claim that matters: the ceiling is NOT a walkable surface, at any z tolerance.
        assert!(col.iter().all(|s| (s.z - (-55.97)).abs() > 1.0),
            "the ceiling must not be a surface at all, got column {col:?}");
        assert_eq!(col.len(), 1, "the corridor column holds exactly one surface (the floor)");
        // Anchoring with the very z that fools the legacy grid (-56.0, the waterline) now lands on
        // the real corridor floor 14u below.
        let s = mesh.nearest_surface(20.0, 20.0, -56.0).expect("a surface exists");
        assert!((s.z - (-69.97)).abs() < 1.0,
            "must anchor to the real floor (-69.97), got {} — a ceiling was treated as a floor", s.z);
    }

    #[test]
    fn stacked_floors_are_both_walkable_in_one_column() {
        // A lower floor and an upper floor 20u above it, both with headroom: the catacombs /
        // multi-storey case (192/497 zones have >10% stacked columns).
        let mut tris = quad(0.0, 0.0, 40.0, 0.0, 40.0);
        tris.extend(quad(20.0, 0.0, 40.0, 0.0, 40.0));
        let mesh = NavMesh::bake(&tris, None, BakeParams::default(), b"t");
        let (c, r) = mesh.to_cell(20.0, 20.0);
        let col = mesh.column(c, r);
        assert_eq!(col.len(), 2, "both floors must exist in the same column: {col:?}");
        assert!((col[0].z - 0.0).abs() < 0.6 && (col[1].z - 20.0).abs() < 0.6);
        // Anchoring picks the tier the caller is actually on — not "the nearest thing to a ray".
        assert!((mesh.nearest_surface(20.0, 20.0, 19.0).unwrap().z - 20.0).abs() < 0.6);
        assert!((mesh.nearest_surface(20.0, 20.0, 1.0).unwrap().z - 0.0).abs() < 0.6);
    }

    #[test]
    fn a_steep_face_is_not_walkable() {
        // A 70° ramp (grade 2.75, well past MAX_WALK_GRADE 1.2) must produce no surface — the legacy
        // grid only applies its grade cap on ASCENT (#313), so it happily plans DOWN such a face.
        let tris = vec![
            [[0.0, 0.0, 0.0], [10.0, 0.0, 27.5], [0.0, 40.0, 0.0]],
            [[10.0, 0.0, 27.5], [10.0, 40.0, 27.5], [0.0, 40.0, 0.0]],
        ];
        let mesh = NavMesh::bake(&tris, None, BakeParams::default(), b"t");
        assert_eq!(mesh.surface_count(), 0, "a 70° face must yield no walkable surface");
    }

    #[test]
    fn water_gets_an_explicit_surface_layer_and_a_swim_route() {
        // A pool: floor at -40, water up to z=0. #197p2 — the legacy grid has NO waterline node, so
        // A* routes the swimmer along the BOTTOM. Here the waterline is a first-class surface.
        let tris = quad(-40.0, 0.0, 60.0, 0.0, 60.0);
        let water = RegionMap::flat_below(0.0);
        let mesh = NavMesh::bake(&tris, Some(&water), BakeParams::default(), b"t");
        let (c, r) = mesh.to_cell(30.0, 30.0);
        let col = mesh.column(c, r);
        assert!(col.iter().any(|s| s.is_swim() && (s.z - 0.0).abs() < 1.5),
            "a swim surface must exist at the waterline: {col:?}");
        assert!(col.iter().any(|s| !s.is_swim()), "the pool bottom is still a surface (divable)");

        // And A* crosses AT THE SURFACE rather than diving to the bottom and back.
        let path = mesh.find_path([10.0, 30.0, 0.0], [50.0, 30.0, 0.0]).expect("a swim route");
        let deepest = path.iter().map(|w| w[2]).fold(f32::MAX, f32::min);
        assert!(deepest > -10.0, "swimmer must stay near the surface, but dove to {deepest}");
    }

    #[test]
    fn cache_roundtrips_and_rejects_a_stale_mesh() {
        let tris = quad(0.0, 0.0, 40.0, 0.0, 40.0);
        let params = BakeParams::default();
        let mesh = NavMesh::bake(&tris, None, params, b"zone-glb-v1");
        let blob = mesh.serialize();

        let back = NavMesh::deserialize(&blob, b"zone-glb-v1", params).expect("roundtrips");
        assert_eq!(back.surface_count(), mesh.surface_count());
        assert_eq!(back.column_count(), mesh.column_count());

        // A changed zone asset invalidates the cache (must re-bake, never path on a stale mesh).
        assert!(NavMesh::deserialize(&blob, b"zone-glb-v2", params).is_none(),
            "a changed source GLB must invalidate the cached mesh");
        // A retuned bake parameter likewise.
        let retuned = BakeParams { agent_radius: 2.0, ..params };
        assert!(NavMesh::deserialize(&blob, b"zone-glb-v1", retuned).is_none(),
            "changed bake params must invalidate the cached mesh");
    }

    #[test]
    fn edges_are_marked_but_never_deleted() {
        // A 40u-wide floor. The outermost ring is within `agent_radius` of the drop-off, so it is
        // FLAGGED (A* pays to use it) — but it must still EXIST. Hard Recast-style erosion deletes
        // it, which measured on the real zones disconnects narrow stairs and bridges and cost 15.5%
        // of the routes the legacy grid can still find.
        let tris = quad(0.0, 0.0, 40.0, 0.0, 40.0);
        let mesh = NavMesh::bake(&tris, None, BakeParams::default(), b"t");

        let (c0, r0) = mesh.to_cell(1.0, 20.0); // hard against the west edge
        let edge = mesh.column(c0, r0);
        assert!(!edge.is_empty(), "the edge surface must SURVIVE (marked, not eroded away)");
        assert!(edge[0].flags & FLAG_EDGE != 0, "the edge surface must be flagged");

        let (c1, r1) = mesh.to_cell(20.0, 20.0); // the middle
        let mid = mesh.column(c1, r1);
        assert!(!mid.is_empty() && mid[0].flags & FLAG_EDGE == 0,
            "the interior must not be flagged as an edge");
    }

    #[test]
    fn a_sealed_area_is_reported_unreachable_instantly_not_by_exhausting_the_search() {
        // Two disconnected floors with a big gap. The legacy planner can only learn this by
        // expanding every node it can reach (the case that stalls the net thread for seconds and
        // trips the linkdead guard). Here it is a component compare: no expansions at all.
        let mut tris = quad(0.0, 0.0, 40.0, 0.0, 40.0);
        tris.extend(quad(0.0, 200.0, 240.0, 0.0, 40.0)); // a separate island, far away
        let mesh = NavMesh::bake(&tris, None, BakeParams::default(), b"t");
        assert!(mesh.find_path([20.0, 20.0, 0.0], [220.0, 20.0, 0.0]).is_none(),
            "a genuinely unreachable goal must be refused");
        // ...and reachable within one island still works.
        assert!(mesh.find_path([5.0, 20.0, 0.0], [35.0, 20.0, 0.0]).is_some());
    }

    #[test]
    fn a_one_way_drop_is_reachable_downward_but_not_back_up() {
        // An upper ledge and a sealed pit 40u below it. You can FALL in; you cannot climb out. The
        // reachability test must model that asymmetry rather than pretending the pit is connected.
        let mut tris = quad(0.0, 0.0, 40.0, 0.0, 40.0);      // upper ledge
        tris.extend(quad(-40.0, 40.0, 80.0, 0.0, 40.0));     // pit floor, 40u down, adjacent
        let mesh = NavMesh::bake(&tris, None, BakeParams::default(), b"t");
        assert!(mesh.find_path([20.0, 20.0, 0.0], [60.0, 20.0, -40.0]).is_some(),
            "you can drop into the pit");
        assert!(mesh.find_path([60.0, 20.0, -40.0], [20.0, 20.0, 0.0]).is_none(),
            "you cannot climb 40u back out — claiming otherwise would be a lie");
    }
}
