//! EQEmu zone BSP region map (`.wtr`) loader + point query.
//!
//! The zone WLD's BSP classifies every point into a region type — used here for two things:
//! swimming (let navigation descend through water where there's no walkable connection) and
//! **zone-line crossing** (detect when the player stands in a `DRNTP` zone-line trigger region).
//!
//! Format: 10-byte magic `"EQEMUWATER"`, u32 version, u32 node_count, then `node_count` fixed
//! records. A point's leaf is found by walking the tree from node 1 (1-based). Region types
//! (EQEmu water_map.h): Normal=0, Water=1, Lava=2, ZoneLine=3, PVP=4, Slime=5, VWater(icy)=7.
//! - **v1** record = 36 bytes: `i32 node_number; f32 normal[3]; f32 split; i32 region; i32 special;
//!   i32 left; i32 right`.
//! - **v2** record = 40 bytes: v1 + trailing `i32 zone_line_index` — the zone-point index of a
//!   zone-line leaf (0 otherwise), which the asset server decodes from the `DRNTP00255<index>_ZONE`
//!   region name. That index matches the `OP_SendZonepoints` `iterator` field, so the client can
//!   resolve where a zone line leads. Both versions are accepted (v1 → every index reads as 0).
//!
//! NOTE the coordinate swap: EQEmu queries the BSP with `(y, x, z)` (see WaterMapV1::ReturnRegionType),
//! so our `is_water(server_x, server_y, server_z)` passes `(server_y, server_x, server_z)` as the
//! BSP location to match the server's classification exactly.

use std::path::Path;

/// EQEmu region type for a zone-line region.
const REGION_ZONE_LINE: i32 = 3;

#[derive(Clone, Copy)]
struct BspNode {
    normal: [f32; 3],
    split:  f32,
    special: i32,
    left:   i32,
    right:  i32,
    /// v2: zone-point index for a zone-line leaf (`special == 3`); 0 otherwise.
    zone_line_index: i32,
}

pub struct RegionMap {
    nodes: Vec<BspNode>,
}

impl RegionMap {
    /// Test-only: a map where everything below `top_z` is water (two-leaf BSP split on z).
    /// Lets nav tests exercise swim edges without a real `.wtr` file.
    #[cfg(test)]
    pub fn flat_below(top_z: f32) -> RegionMap {
        RegionMap { nodes: vec![
            // node 1: dist = z - top_z; above → leaf 2 (dry), below → leaf 3 (water)
            BspNode { normal: [0.0, 0.0, 1.0], split: -top_z, special: 0, left: 2, right: 3, zone_line_index: 0 },
            BspNode { normal: [0.0; 3], split: 0.0, special: 0, left: 0, right: 0, zone_line_index: 0 },
            BspNode { normal: [0.0; 3], split: 0.0, special: 1, left: 0, right: 0, zone_line_index: 0 },
        ]}
    }

    /// Test-only: water only inside the XY box `[n0,n1] x [e0,e1]` and below `top_z` — everywhere
    /// else (including below `top_z` outside the box) reads as dry. Unlike `flat_below` (a single
    /// z-split, water at that depth EVERYWHERE), this lets a nav test build a spatially bounded
    /// pond so swim edges can't misfire against unrelated dry terrain elsewhere in the scene.
    #[cfg(test)]
    pub fn box_below(n0: f32, n1: f32, e0: f32, e1: f32, top_z: f32) -> RegionMap {
        let dry   = BspNode { normal: [0.0; 3], split: 0.0, special: 0, left: 0, right: 0, zone_line_index: 0 };
        let water = BspNode { normal: [0.0; 3], split: 0.0, special: 1, left: 0, right: 0, zone_line_index: 0 };
        RegionMap { nodes: vec![
            // 1: north >= n0 → 2, else dry(6)
            BspNode { normal: [1.0, 0.0, 0.0], split: -n0, special: 0, left: 2, right: 6, zone_line_index: 0 },
            // 2: north <= n1 → 3, else dry(6)
            BspNode { normal: [-1.0, 0.0, 0.0], split: n1, special: 0, left: 3, right: 6, zone_line_index: 0 },
            // 3: east >= e0 → 4, else dry(6)
            BspNode { normal: [0.0, 1.0, 0.0], split: -e0, special: 0, left: 4, right: 6, zone_line_index: 0 },
            // 4: east <= e1 → 5, else dry(6)
            BspNode { normal: [0.0, -1.0, 0.0], split: e1, special: 0, left: 5, right: 6, zone_line_index: 0 },
            // 5: up < top_z → water(7), else dry(6)
            BspNode { normal: [0.0, 0.0, 1.0], split: -top_z, special: 0, left: 6, right: 7, zone_line_index: 0 },
            dry,   // 6
            water, // 7
        ]}
    }

    /// Test-only: a map where everything below `top_z` is a zone-line region carrying `index`.
    #[cfg(test)]
    pub fn zone_line_below(top_z: f32, index: i32) -> RegionMap {
        RegionMap { nodes: vec![
            BspNode { normal: [0.0, 0.0, 1.0], split: -top_z, special: 0, left: 2, right: 3, zone_line_index: 0 },
            BspNode { normal: [0.0; 3], split: 0.0, special: 0, left: 0, right: 0, zone_line_index: 0 },
            BspNode { normal: [0.0; 3], split: 0.0, special: REGION_ZONE_LINE, left: 0, right: 0, zone_line_index: index },
        ]}
    }

    /// Load `<dir>/<zone>.wtr` (v1 or v2 BSP). Returns None if missing or unparseable (nav then just
    /// behaves as before — no water descents, no region-based zone crossing).
    pub fn load(dir: &Path, zone: &str) -> Option<RegionMap> {
        let path = dir.join(format!("{zone}.wtr"));
        let d = std::fs::read(&path).ok()?;
        if d.len() < 14 || &d[..10] != b"EQEMUWATER" { return None; }
        let version = u32::from_le_bytes(d[10..14].try_into().ok()?);
        // v1 = 36-byte nodes (no zone-line index); v2 = 40-byte nodes (trailing index).
        let stride = match version {
            1 => 36,
            2 => 40,
            _ => { tracing::warn!("region_map: {} is v{version}, only v1/v2 supported", path.display()); return None; }
        };
        let mut off = 14;
        let count = u32::from_le_bytes(d[off..off + 4].try_into().ok()?) as usize; off += 4;
        if d.len() < off + count * stride { return None; }
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            // ZBSP_Node: i32 node_number, f32 normal[3], f32 split, i32 region, i32 special, i32 left, i32 right [, i32 zone_line_index]
            let rd_f = |o: usize| f32::from_le_bytes(d[o..o + 4].try_into().unwrap());
            let rd_i = |o: usize| i32::from_le_bytes(d[o..o + 4].try_into().unwrap());
            nodes.push(BspNode {
                normal:  [rd_f(off + 4), rd_f(off + 8), rd_f(off + 12)],
                split:   rd_f(off + 16),
                special: rd_i(off + 24),
                left:    rd_i(off + 28),
                right:   rd_i(off + 32),
                zone_line_index: if stride == 40 { rd_i(off + 36) } else { 0 },
            });
            off += stride;
        }
        tracing::info!("region_map: loaded {} (v{version}, {} BSP nodes)", path.display(), nodes.len());
        Some(RegionMap { nodes })
    }

    /// Walk the BSP from node 1 to the leaf containing the server-coord point. The swap to (y,x,z)
    /// matches EQEmu's WaterMapV1::ReturnRegionType. Returns `None` if the tree is empty or the walk
    /// falls off (degenerate node / missing child).
    fn leaf_at(&self, sx: f32, sy: f32, sz: f32) -> Option<&BspNode> {
        if self.nodes.is_empty() { return None; }
        let (lx, ly, lz) = (sy, sx, sz);
        let mut nn: i32 = 1;
        for _ in 0..256 { // depth guard
            let node = self.nodes.get((nn - 1) as usize)?;
            if node.left == 0 && node.right == 0 { return Some(node); }
            let dist = lx * node.normal[0] + ly * node.normal[1] + lz * node.normal[2] + node.split;
            if dist == 0.0 { return None; }
            nn = if dist > 0.0 { node.left } else { node.right };
            if nn == 0 { return None; }
        }
        None
    }

    /// Region type at a server-coord point. 1=Water, 2=Lava, 3=ZoneLine, 7=VWater.
    fn region_type(&self, sx: f32, sy: f32, sz: f32) -> i32 {
        self.leaf_at(sx, sy, sz).map(|n| n.special).unwrap_or(0)
    }

    /// True if the point is in liquid you can swim through (water or icy water).
    pub fn is_water(&self, sx: f32, sy: f32, sz: f32) -> bool {
        matches!(self.region_type(sx, sy, sz), 1 | 7)
    }

    /// If the point is inside a zone-line (`DRNTP`) region, the zone-point index it carries — the
    /// same value as the `OP_SendZonepoints` `iterator` for that line, used to resolve the
    /// destination zone. `None` when the point isn't in a zone-line region, or when loaded from a
    /// v1 map (which carries no index). A zero index (unresolved/short-form region) reads as `None`.
    pub fn zone_line_at(&self, sx: f32, sy: f32, sz: f32) -> Option<i32> {
        let node = self.leaf_at(sx, sy, sz)?;
        if node.special == REGION_ZONE_LINE && node.zone_line_index != 0 {
            Some(node.zone_line_index)
        } else {
            None
        }
    }

    /// Distinct zone-point indices of every zone-line (`DRNTP`) region in this map — the set of
    /// exits the current zone has. Each index links to an entrance via the `OP_SendZonepoints`
    /// `iterator`. Empty for a v1 map (no indices).
    pub fn zone_line_indices(&self) -> Vec<i32> {
        let mut v: Vec<i32> = self
            .nodes
            .iter()
            .filter(|n| n.left == 0 && n.right == 0 && n.special == REGION_ZONE_LINE && n.zone_line_index != 0)
            .map(|n| n.zone_line_index)
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// Precompute a representative interior `[east, north, z]` point for every zone-line region, so
    /// "where is the line to zone X?" is an O(1) lookup at request time instead of an exhaustive
    /// runtime scan that stalled the network thread and linkdead-ed the client (#204). Runs ONCE at
    /// zone load (off the net thread). `bounds` = `(min_e, max_e, min_n, max_n, zmin, zmax)` (server
    /// coords). Returns `(zone_line_index, [east, north, z])` per zone-line leaf.
    ///
    /// Walks the BSP once (O(nodes)), carrying an AABB tightened by each axis-aligned split plane
    /// (oblique planes are left as a superset), then grid-samples inside each zone-line leaf's AABB
    /// and validates every candidate with `zone_line_at`, so a slightly loose AABB can never yield a
    /// wrong point — only fail to find one (logged). Sample count per leaf is capped, so the whole
    /// pass is bounded regardless of zone size.
    pub fn zone_line_region_points(&self, bounds: (f32, f32, f32, f32, f32, f32)) -> Vec<(i32, [f32; 3])> {
        let (min_e, max_e, min_n, max_n, zmin, zmax) = bounds;
        if self.nodes.is_empty() || min_e > max_e || min_n > max_n || zmin > zmax { return Vec::new(); }
        // BSP axes (leaf_at swaps to `(sy, sx, sz)`): axis0 = north, axis1 = east, axis2 = z.
        let root = [[min_n, max_n], [min_e, max_e], [zmin, zmax]];
        let mut out = Vec::new();
        let mut obliques: Vec<([f32; 3], f32, bool)> = Vec::new();
        self.collect_zone_line_leaves(1, root, &mut obliques, 0, &mut out);
        out
    }

    /// Tighten `aabb` to a plane's half-space for an axis-aligned plane; leave it unchanged (a
    /// superset) for an oblique one. `positive` selects the `dist > 0` side.
    fn tighten_aabb(mut aabb: [[f32; 2]; 3], normal: [f32; 3], split: f32, positive: bool) -> [[f32; 2]; 3] {
        // Only axis-aligned planes tighten an AABB cleanly.
        let axis = (0..3).find(|&k| normal[k].abs() > 1e-4 && (0..3).all(|j| j == k || normal[j].abs() < 1e-4));
        if let Some(k) = axis {
            let bound = -split / normal[k]; // coord[k] on the plane
            if (normal[k] > 0.0) == positive { aabb[k][0] = aabb[k][0].max(bound); } // coord > bound
            else                             { aabb[k][1] = aabb[k][1].min(bound); } // coord < bound
        }
        aabb
    }

    /// True if `normal` is axis-aligned (captured exactly by the AABB); false = oblique.
    fn is_axis_aligned(normal: [f32; 3]) -> bool {
        (0..3).any(|k| normal[k].abs() > 1e-4 && (0..3).all(|j| j == k || normal[j].abs() < 1e-4))
    }

    /// DFS the BSP once, carrying the AABB (tightened by axis-aligned split planes) AND the list of
    /// OBLIQUE half-space constraints on the current leaf (which the AABB can't represent). At each
    /// zone-line leaf, find a point inside its convex region for the O(1) find_zone_line_near lookup.
    fn collect_zone_line_leaves(&self, nn: i32, aabb: [[f32; 2]; 3],
        obliques: &mut Vec<([f32; 3], f32, bool)>, depth: u32, out: &mut Vec<(i32, [f32; 3])>) {
        if nn <= 0 || depth > 256 { return; }
        let Some(node) = self.nodes.get((nn - 1) as usize) else { return; };
        if node.left == 0 && node.right == 0 {
            if node.special == REGION_ZONE_LINE && node.zone_line_index != 0 {
                match self.find_interior_point(node.zone_line_index, aabb, obliques) {
                    Some(p) => out.push((node.zone_line_index, p)),
                    None => tracing::warn!(
                        "region_map: no interior point found for zone-line index {} (AABB {:?}) — /zone_cross to it may fail",
                        node.zone_line_index, aabb),
                }
            }
            return;
        }
        let oblique = !Self::is_axis_aligned(node.normal);
        // left child: dist > 0 (positive side); right child: dist < 0.
        for (child, positive) in [(node.left, true), (node.right, false)] {
            let child_aabb = Self::tighten_aabb(aabb, node.normal, node.split, positive);
            if oblique { obliques.push((node.normal, node.split, positive)); }
            self.collect_zone_line_leaves(child, child_aabb, obliques, depth + 1, out);
            if oblique { obliques.pop(); }
        }
    }

    /// Find a point inside a zone-line leaf's convex region — the intersection of its AABB (axis-
    /// aligned planes) and the `obliques` half-spaces. Start at the AABB centre and iteratively
    /// project onto any violated oblique constraint (re-clamping into the AABB each round), which
    /// converges to an interior point of the convex region. Validate with `zone_line_at`; if that
    /// somehow fails, fall back to the old grid sample. Returns `[east, north, z]`. #230
    fn find_interior_point(&self, index: i32, aabb: [[f32; 2]; 3], obliques: &[([f32; 3], f32, bool)]) -> Option<[f32; 3]> {
        if (0..3).any(|k| aabb[k][1] < aabb[k][0]) { return None; }
        // BSP space p = (north, east, z) (leaf_at's (sy,sx,sz) swap).
        let mut p = [(aabb[0][0] + aabb[0][1]) * 0.5, (aabb[1][0] + aabb[1][1]) * 0.5, (aabb[2][0] + aabb[2][1]) * 0.5];
        const MARGIN: f32 = 1.0; // aim a little inside the plane, not exactly on it
        for _ in 0..48 {
            let mut moved = false;
            for &(n, s, positive) in obliques {
                let d = n[0] * p[0] + n[1] * p[1] + n[2] * p[2] + s;
                let inside = if positive { d > 0.0 } else { d < 0.0 };
                if !inside {
                    let n2 = n[0] * n[0] + n[1] * n[1] + n[2] * n[2];
                    if n2 < 1e-9 { continue; }
                    let target = if positive { MARGIN } else { -MARGIN };
                    let t = (target - d) / n2;
                    p[0] += n[0] * t; p[1] += n[1] * t; p[2] += n[2] * t;
                    moved = true;
                }
            }
            for k in 0..3 { p[k] = p[k].clamp(aabb[k][0], aabb[k][1]); }
            if !moved { break; }
        }
        let cand = [p[1], p[0], p[2]]; // (east, north, z)
        if self.zone_line_at(cand[0], cand[1], cand[2]) == Some(index) {
            return Some(cand);
        }
        self.sample_region_point(index, aabb)
    }

    /// Grid-sample `aabb` (`[north, east, z]` ranges) for points classifying as zone-line `index`;
    /// return their centroid as `[east, north, z]`. Step is chosen to cap total samples (bounded
    /// work even for a loose AABB). `None` if the region isn't hit (too small for the step).
    fn sample_region_point(&self, index: i32, aabb: [[f32; 2]; 3]) -> Option<[f32; 3]> {
        let ext = [aabb[0][1] - aabb[0][0], aabb[1][1] - aabb[1][0], aabb[2][1] - aabb[2][0]];
        if ext.iter().any(|&e| e < 0.0) { return None; }
        const MAX_SAMPLES: f64 = 120_000.0;
        let vol = ext[0].max(1.0) as f64 * ext[1].max(1.0) as f64 * ext[2].max(1.0) as f64;
        let step = ((vol / MAX_SAMPLES).cbrt() as f32).clamp(2.0, 64.0);
        let (mut sum, mut hits) = ([0f64; 3], 0u32);
        let mut north = aabb[0][0];
        while north <= aabb[0][1] {
            let mut east = aabb[1][0];
            while east <= aabb[1][1] {
                let mut z = aabb[2][0];
                while z <= aabb[2][1] {
                    if self.zone_line_at(east, north, z) == Some(index) {
                        sum[0] += east as f64; sum[1] += north as f64; sum[2] += z as f64; hits += 1;
                    }
                    z += step;
                }
                east += step;
            }
            north += step;
        }
        (hits > 0).then(|| [(sum[0] / hits as f64) as f32, (sum[1] / hits as f64) as f32, (sum[2] / hits as f64) as f32])
    }

    /// Height of the water surface directly above a submerged point `(sx, sy, submerged_z)`.
    /// Binary-searches upward for the water→air boundary. Returns `None` if the point isn't in
    /// water, or if it's still water `MAX_UP` above it (unbounded / not a normal surface). Used by
    /// the controller's buoyancy so a character floats up to the surface instead of sinking (#172).
    pub fn surface_z(&self, sx: f32, sy: f32, submerged_z: f32) -> Option<f32> {
        if !self.is_water(sx, sy, submerged_z) { return None; }
        const MAX_UP: f32 = 200.0;
        let mut lo = submerged_z;            // in water
        let mut hi = submerged_z + MAX_UP;   // expected air
        if self.is_water(sx, sy, hi) { return None; } // still water at the top → not a normal surface
        for _ in 0..24 {
            let mid = (lo + hi) * 0.5;
            if self.is_water(sx, sy, mid) { lo = mid; } else { hi = mid; }
        }
        Some(hi) // first non-water height ≈ the surface
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a v2 `.wtr` byte blob for the given leaf records so the loader path is exercised.
    /// Each `node` is (normal, split, special, left, right, zone_line_index).
    fn wtr_v2(nodes: &[([f32; 3], f32, i32, i32, i32, i32)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(b"EQEMUWATER");
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        for (i, (normal, split, special, left, right, zli)) in nodes.iter().enumerate() {
            out.extend_from_slice(&(i as i32).to_le_bytes());
            for c in normal { out.extend_from_slice(&c.to_le_bytes()); }
            out.extend_from_slice(&split.to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes()); // region ordinal (unused by the reader)
            out.extend_from_slice(&special.to_le_bytes());
            out.extend_from_slice(&left.to_le_bytes());
            out.extend_from_slice(&right.to_le_bytes());
            out.extend_from_slice(&zli.to_le_bytes());
        }
        out
    }

    #[test]
    fn v2_load_exposes_zone_line_index() {
        // node 1 splits on z: above top_z (2.0) → dry leaf 2, below → zone-line leaf 3 index 1.
        let blob = wtr_v2(&[
            ([0.0, 0.0, 1.0], -2.0, 0, 2, 3, 0),
            ([0.0; 3], 0.0, 0, 0, 0, 0),
            ([0.0; 3], 0.0, REGION_ZONE_LINE, 0, 0, 1),
        ]);
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("z.wtr"), &blob).unwrap();
        let rm = RegionMap::load(dir.path(), "z").expect("v2 loads");
        // A point below the split is in the zone-line region carrying index 1.
        assert_eq!(rm.zone_line_at(10.0, 20.0, -5.0), Some(1));
        // A point above is not a zone line.
        assert_eq!(rm.zone_line_at(10.0, 20.0, 5.0), None);
        // The zone-line leaf isn't water.
        assert!(!rm.is_water(10.0, 20.0, -5.0));
    }

    #[test]
    fn v1_load_has_no_zone_line_index() {
        // A v1 blob (36-byte nodes) must still load, with every zone_line_index reading as 0/None.
        let mut out = Vec::new();
        out.extend_from_slice(b"EQEMUWATER");
        out.extend_from_slice(&1u32.to_le_bytes());
        let nodes: &[([f32; 3], f32, i32, i32, i32)] = &[
            ([0.0, 0.0, 1.0], -2.0, 0, 2, 3),
            ([0.0; 3], 0.0, 0, 0, 0),
            ([0.0; 3], 0.0, 1, 0, 0), // water leaf
        ];
        out.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        for (i, (normal, split, special, left, right)) in nodes.iter().enumerate() {
            out.extend_from_slice(&(i as i32).to_le_bytes());
            for c in normal { out.extend_from_slice(&c.to_le_bytes()); }
            out.extend_from_slice(&split.to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes());
            out.extend_from_slice(&special.to_le_bytes());
            out.extend_from_slice(&left.to_le_bytes());
            out.extend_from_slice(&right.to_le_bytes());
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("z.wtr"), &out).unwrap();
        let rm = RegionMap::load(dir.path(), "z").expect("v1 still loads");
        assert!(rm.is_water(10.0, 20.0, -5.0));
        assert_eq!(rm.zone_line_at(10.0, 20.0, -5.0), None); // v1 carries no index
    }

    #[test]
    fn zone_line_below_helper_reports_index() {
        let rm = RegionMap::zone_line_below(0.0, 7);
        assert_eq!(rm.zone_line_at(1.0, 2.0, -3.0), Some(7));
        assert_eq!(rm.zone_line_at(1.0, 2.0, 3.0), None);
    }

    #[test]
    fn zone_line_indices_enumerates_distinct_indices() {
        assert_eq!(RegionMap::zone_line_below(0.0, 7).zone_line_indices(), vec![7]);
        // A water-only map (no zone-line leaves) has no exit indices.
        assert!(RegionMap::flat_below(0.0).zone_line_indices().is_empty());
    }

    #[test]
    fn zone_line_region_points_finds_a_validated_interior_point() {
        // zone-line region = everywhere z < 0. Precompute must return one point actually inside it.
        let rm = RegionMap::zone_line_below(0.0, 7);
        let pts = rm.zone_line_region_points((-100.0, 100.0, -50.0, 50.0, -30.0, 20.0));
        assert_eq!(pts.len(), 1, "one zone-line region");
        let (idx, p) = pts[0];
        assert_eq!(idx, 7);
        assert!(p[2] < 0.0, "point is below the z=0 boundary (inside the region): {p:?}");
        // Ground-truth: the returned [east, north, z] classifies as the zone-line region.
        assert_eq!(rm.zone_line_at(p[0], p[1], p[2]), Some(7), "returned point validates: {p:?}");
        // A water-only map yields no zone-line region points.
        assert!(RegionMap::flat_below(0.0)
            .zone_line_region_points((-100.0, 100.0, -50.0, 50.0, -30.0, 20.0)).is_empty());
    }

    #[test]
    fn find_interior_point_handles_oblique_region() {
        // Zone-line region = the half-space north+east > 50, bounded by an OBLIQUE plane the AABB
        // can't represent. The AABB centre (0,0) is on the WRONG side, so constraint projection must
        // move the candidate into the region (a coarse grid could otherwise miss a thin slice). #230
        let rm = RegionMap { nodes: vec![
            // node1: dist = north + east - 50; >0 → leaf2 (zone-line idx 7), <0 → leaf3 (normal)
            BspNode { normal: [1.0, 1.0, 0.0], split: -50.0, special: 0, left: 2, right: 3, zone_line_index: 0 },
            BspNode { normal: [0.0; 3], split: 0.0, special: REGION_ZONE_LINE, left: 0, right: 0, zone_line_index: 7 },
            BspNode { normal: [0.0; 3], split: 0.0, special: 0, left: 0, right: 0, zone_line_index: 0 },
        ] };
        let pts = rm.zone_line_region_points((-100.0, 100.0, -100.0, 100.0, -10.0, 10.0));
        assert_eq!(pts.len(), 1, "one zone-line region");
        let (idx, p) = pts[0];
        assert_eq!(idx, 7);
        // p = [east, north, z]; must be inside the oblique region and classify correctly.
        assert!(p[0] + p[1] > 50.0, "point inside north+east>50: {p:?}");
        assert_eq!(rm.zone_line_at(p[0], p[1], p[2]), Some(7), "returned point validates: {p:?}");
    }
}
