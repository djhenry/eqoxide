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
}
