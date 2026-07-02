//! EQEmu water-region map (`.wtr`) loader + point query, used to let navigation descend through
//! water (swim down a canal/shaft to a lower level) where there is no walkable connection.
//!
//! Format (v1, the Titanium-era zones like qcat): 10-byte magic "EQEMUWATER", u32 version, then a
//! BSP tree: u32 node_count followed by `node_count` fixed 36-byte `ZBSP_Node` records. A point's
//! region type is found by walking the tree from node 1 (1-based). Region types (EQEmu
//! water_map.h): Normal=0, Water=1, Lava=2, PVP=4, VWater(icy)=7.
//!
//! NOTE the coordinate swap: EQEmu queries the BSP with `(y, x, z)` (see WaterMapV1::ReturnRegionType),
//! so our `is_water(server_x, server_y, server_z)` passes `(server_y, server_x, server_z)` as the
//! BSP location to match the server's classification exactly.

use std::path::Path;

#[derive(Clone, Copy)]
struct BspNode {
    normal: [f32; 3],
    split:  f32,
    special: i32,
    left:   i32,
    right:  i32,
}

pub struct WaterMap {
    nodes: Vec<BspNode>,
}

impl WaterMap {
    /// Test-only: a map where everything below `top_z` is water (two-leaf BSP split on z).
    /// Lets nav tests exercise swim edges without a real `.wtr` file.
    #[cfg(test)]
    pub fn flat_below(top_z: f32) -> WaterMap {
        WaterMap { nodes: vec![
            // node 1: dist = z - top_z; above → leaf 2 (dry), below → leaf 3 (water)
            BspNode { normal: [0.0, 0.0, 1.0], split: -top_z, special: 0, left: 2, right: 3 },
            BspNode { normal: [0.0; 3], split: 0.0, special: 0, left: 0, right: 0 },
            BspNode { normal: [0.0; 3], split: 0.0, special: 1, left: 0, right: 0 },
        ]}
    }

    /// Load `<dir>/<zone>.wtr` (v1 BSP). Returns None if missing or unparseable (nav then just
    /// behaves as before — no water descents).
    pub fn load(dir: &Path, zone: &str) -> Option<WaterMap> {
        let path = dir.join(format!("{zone}.wtr"));
        let d = std::fs::read(&path).ok()?;
        if d.len() < 14 || &d[..10] != b"EQEMUWATER" { return None; }
        let version = u32::from_le_bytes(d[10..14].try_into().ok()?);
        if version != 1 { tracing::warn!("water_map: {} is v{version}, only v1 supported", path.display()); return None; }
        let mut off = 14;
        let count = u32::from_le_bytes(d[off..off + 4].try_into().ok()?) as usize; off += 4;
        if d.len() < off + count * 36 { return None; }
        let mut nodes = Vec::with_capacity(count);
        for _ in 0..count {
            // ZBSP_Node: i32 node_number, f32 normal[3], f32 split, i32 region, i32 special, i32 left, i32 right
            let rd_f = |o: usize| f32::from_le_bytes(d[o..o + 4].try_into().unwrap());
            let rd_i = |o: usize| i32::from_le_bytes(d[o..o + 4].try_into().unwrap());
            nodes.push(BspNode {
                normal:  [rd_f(off + 4), rd_f(off + 8), rd_f(off + 12)],
                split:   rd_f(off + 16),
                special: rd_i(off + 24),
                left:    rd_i(off + 28),
                right:   rd_i(off + 32),
            });
            off += 36;
        }
        tracing::info!("water_map: loaded {} ({} BSP nodes)", path.display(), nodes.len());
        Some(WaterMap { nodes })
    }

    /// Region type at a server-coord point (the swap to (y,x,z) matches EQEmu). 1=Water, 7=VWater.
    fn region_type(&self, sx: f32, sy: f32, sz: f32) -> i32 {
        if self.nodes.is_empty() { return 0; }
        // BSP location is (y, x, z) per WaterMapV1::ReturnRegionType.
        let (lx, ly, lz) = (sy, sx, sz);
        let mut nn: i32 = 1;
        for _ in 0..256 { // depth guard
            let node = match self.nodes.get((nn - 1) as usize) { Some(n) => n, None => return 0 };
            if node.left == 0 && node.right == 0 { return node.special; }
            let dist = lx * node.normal[0] + ly * node.normal[1] + lz * node.normal[2] + node.split;
            if dist == 0.0 { return 0; }
            nn = if dist > 0.0 { node.left } else { node.right };
            if nn == 0 { return 0; }
        }
        0
    }

    /// True if the point is in liquid you can swim through (water or icy water).
    pub fn is_water(&self, sx: f32, sy: f32, sz: f32) -> bool {
        matches!(self.region_type(sx, sy, sz), 1 | 7)
    }
}
