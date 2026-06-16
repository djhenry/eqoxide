use anyhow::{Context, Result};
use std::path::Path;

/// CPU-side mesh data ready for GPU upload.
#[derive(Clone)]
pub struct MeshData {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub uvs: Vec<[f32; 2]>,
    pub indices: Vec<u32>,
    pub texture_name: Option<String>,
    /// glTF pbrMetallicRoughness.baseColorFactor, or [1,1,1,1] if absent.
    pub base_color: [f32; 4],
    /// World-space offset: add to each position to get absolute coordinates.
    pub center: [f32; 3],
}

/// CPU-side texture data ready for GPU upload.
#[derive(Clone)]
pub struct TextureData {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// All CPU-side data for a zone, loaded from .s3d + .wld.
#[derive(Clone)]
pub struct ZoneAssets {
    pub meshes: Vec<MeshData>,
    pub textures: Vec<TextureData>,
}

impl ZoneAssets {
    /// Compute the 2D bounding box of all mesh vertices.
    /// Returns `([min_east, min_north], [max_east, max_north])` in map coordinates
    /// (east = server_y = map_x, north = server_x = map_y).
    /// libeq_wld convention: position = [east, height, north].
    pub fn bounds_xy(&self) -> Option<([f32; 2], [f32; 2])> {
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        for m in &self.meshes {
            for p in &m.positions {
                let e = p[0] + m.center[0]; // east
                let n = p[2] + m.center[2]; // north
                if e < min[0] { min[0] = e; }
                if n < min[1] { min[1] = n; }
                if e > max[0] { max[0] = e; }
                if n > max[1] { max[1] = n; }
            }
        }
        if min[0] == f32::MAX { None } else { Some((min, max)) }
    }
}

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

pub struct Collision {
    tris:      Vec<[[f32; 3]; 3]>,
    cells:     Vec<Vec<u32>>,
    origin:    [f32; 2], // (east, north) of cell (0,0) corner
    cell_size: f32,
    cols:      usize,
    rows:      usize,
}

impl Collision {
    /// Build the grid from zone geometry. `cell_size` is in EQ units.
    pub fn build(assets: &ZoneAssets, cell_size: f32) -> Self {
        // Flatten every triangle into world space [east, north, height].
        let mut tris: Vec<[[f32; 3]; 3]> = Vec::new();
        for m in &assets.meshes {
            let pos = &m.positions;
            let idx = &m.indices;
            let mut k = 0;
            while k + 2 < idx.len() {
                let (ia, ib, ic) = (idx[k] as usize, idx[k + 1] as usize, idx[k + 2] as usize);
                k += 3;
                if ia >= pos.len() || ib >= pos.len() || ic >= pos.len() { continue; }
                // libeq [east, height, north] -> world [east, north, height]
                tris.push([
                    [pos[ia][0] + m.center[0], pos[ia][2] + m.center[2], pos[ia][1] + m.center[1]],
                    [pos[ib][0] + m.center[0], pos[ib][2] + m.center[2], pos[ib][1] + m.center[1]],
                    [pos[ic][0] + m.center[0], pos[ic][2] + m.center[2], pos[ic][1] + m.center[1]],
                ]);
            }
        }

        // XY bounds.
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        for t in &tris {
            for v in t {
                if v[0] < min[0] { min[0] = v[0]; }
                if v[1] < min[1] { min[1] = v[1]; }
                if v[0] > max[0] { max[0] = v[0]; }
                if v[1] > max[1] { max[1] = v[1]; }
            }
        }
        let cell_size = cell_size.max(1.0);
        if tris.is_empty() || min[0] == f32::MAX {
            return Collision { tris, cells: vec![], origin: [0.0, 0.0], cell_size, cols: 0, rows: 0 };
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
        Collision { tris, cells, origin: min, cell_size, cols, rows }
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
    /// undersides) have negative t and are never returned. Vertical walls produce
    /// det ≈ 0 and are skipped, so standing next to a wall doesn't pull the floor
    /// up to wall height.
    pub fn floor_z(&self, east: f32, north: f32, fallback: f32) -> f32 {
        if self.cols == 0 { return fallback; }
        // Start 2 units above the player (absorbs server-z / visual-z discrepancy)
        // and cast 100 units straight down — ample range for any EQ zone geometry.
        let ray_start = [east, north, fallback + 2.0];
        let ray_end   = [east, north, fallback - 100.0];
        match self.nearest_hit_t(ray_start, ray_end) {
            Some(t) => ray_start[2] + t * (ray_end[2] - ray_start[2]),
            None    => fallback,
        }
    }

    /// Nearest geometry hit along segment `from → to`, as fraction `t ∈ (0,1]`.
    /// Both points are GPU world space `[east, north, height]`. Möller–Trumbore.
    pub fn nearest_hit_t(&self, from: [f32; 3], to: [f32; 3]) -> Option<f32> {
        if self.cols == 0 { return None; }
        let dir = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        if dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2] < 1e-6 { return None; }
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
                    if u < 0.0 || u > 1.0 { continue; }
                    let q = cross(tvec, e1);
                    let v = dot(dir, q) * inv;
                    if v < 0.0 || u + v > 1.0 { continue; }
                    let t = dot(e2, q) * inv;
                    if t > 1e-3 && t <= 1.0 && best.map_or(true, |b| t < b) {
                        best = Some(t);
                    }
                }
            }
        }
        best
    }

    /// Is `from → to` blocked by geometry before ~92% of the way? Used for nameplate
    /// occlusion; the cutoff keeps the NPC's own feet/floor from counting as occluders.
    pub fn segment_blocked(&self, from: [f32; 3], to: [f32; 3]) -> bool {
        self.nearest_hit_t(from, to).map_or(false, |t| t < 0.92)
    }

    /// Can the player step from `from` to `to` without crossing a wall?
    ///
    /// The ray is extended past `to` by `radius` so the player stops a little short of
    /// the wall instead of clipping into it. Caller should pass points at roughly chest
    /// height (a couple units above the feet) so knee-high floor lips and stair edges
    /// don't read as walls. Returns `true` (clear) when there is no zone geometry loaded.
    pub fn path_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if dist < 1e-5 { return true; }
        let ext = (dist + radius.max(0.0)) / dist;
        let target = [from[0] + d[0] * ext, from[1] + d[1] * ext, from[2] + d[2] * ext];
        self.nearest_hit_t(from, target).is_none()
    }
}

impl ZoneAssets {
    /// Load zone geometry and textures from an S3D archive.
    /// Skips unrecognised fragments with a warning instead of returning Err.
    pub fn load(s3d_path: &Path) -> Result<Self> {
        // Step 1: Open the S3D archive with libeq_pfs.
        let file = std::fs::File::open(s3d_path)
            .with_context(|| format!("failed to open S3D archive: {}", s3d_path.display()))?;
        let mut pfs = libeq_pfs::PfsReader::open(file)
            .with_context(|| format!("failed to parse PFS archive: {}", s3d_path.display()))?;

        // Step 2: Determine the .wld filename (same stem as the .s3d).
        let stem = s3d_path
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| "S3D path has no file stem")?;
        let wld_name = format!("{}.wld", stem);

        // Step 3: Extract .wld bytes.
        let wld_bytes = pfs
            .get(&wld_name)
            .with_context(|| format!("failed to read {} from archive", wld_name))?
            .with_context(|| format!("{} not found inside archive", wld_name))?;

        // Step 4: Parse the WLD file.
        // NOTE: libeq_wld::load() calls Wld::load() which panics on parse error.
        // Wrapping in catch_unwind is fragile, so we let the panic propagate.
        let wld = libeq_wld::load(&wld_bytes)
            .map_err(|e| anyhow::anyhow!("failed to parse WLD: {}", e))?;

        // Step 5: Collect all filenames for texture lookup.
        let filenames = pfs
            .filenames()
            .with_context(|| "failed to list archive filenames")?;

        // Step 6: Build mesh data, splitting by primitive so each sub-mesh
        // gets the correct material texture.  A single WLD mesh can use
        // multiple materials (e.g. cobblestone + water + walls); the old
        // code only took the first material and applied it everywhere.
        let mut meshes = Vec::new();
        for mesh in wld.meshes() {
            let all_positions = mesh.positions();
            if all_positions.is_empty() {
                continue;
            }

            let (cx, cy, cz) = mesh.center();
            let all_normals = mesh.normals();
            let all_uvs = mesh.texture_coordinates();

            for primitive in mesh.primitives() {
                let prim_indices: Vec<u32> = primitive.indices();
                if prim_indices.is_empty() {
                    continue;
                }

                let positions: Vec<[f32; 3]> = prim_indices
                    .iter()
                    .map(|&i| {
                        let p = all_positions[i as usize];
                        [p[0], p[1], p[2]]
                    })
                    .collect();
                let normals: Vec<[f32; 3]> = prim_indices
                    .iter()
                    .map(|&i| {
                        all_normals
                            .get(i as usize)
                            .copied()
                            .unwrap_or([0.0, 0.0, 1.0])
                    })
                    .collect();
                let uvs: Vec<[f32; 2]> = prim_indices
                    .iter()
                    .map(|&i| {
                        all_uvs
                            .get(i as usize)
                            .copied()
                            .unwrap_or([0.0, 0.0])
                    })
                    .collect();

                let material = primitive.material();
                let texture_name = material
                    .base_color_texture()
                    .and_then(|t| t.source());

                meshes.push(MeshData {
                    positions,
                    normals,
                    uvs,
                    indices: (0..prim_indices.len() as u32).collect(),
                    texture_name,
                    base_color: [1.0, 1.0, 1.0, 1.0],
                    center: [cx, cy, cz],
                });
            }
        }

        // Step 7: Load BMP textures from the archive.
        let mut textures = Vec::new();
        for filename in &filenames {
            if !filename.to_lowercase().ends_with(".bmp") {
                continue;
            }
            let bmp_bytes = match pfs.get(filename) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    eprintln!("warning: texture {} listed but not found in archive", filename);
                    continue;
                }
                Err(e) => {
                    eprintln!("warning: failed to read texture {}: {}", filename, e);
                    continue;
                }
            };

            match image::load_from_memory_with_format(&bmp_bytes, image::ImageFormat::Bmp) {
                Ok(img) => {
                    let rgba_img = img.to_rgba8();
                    let (width, height) = rgba_img.dimensions();
                    textures.push(TextureData {
                        name: filename.clone(),
                        width,
                        height,
                        rgba: rgba_img.into_raw(),
                    });
                }
                Err(e) => {
                    eprintln!("warning: failed to decode texture {}: {}", filename, e);
                }
            }
        }

        Ok(ZoneAssets { meshes, textures })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn load_returns_err_on_missing_file() {
        let result = ZoneAssets::load(Path::new("/nonexistent/path.s3d"));
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "requires real extracted assets at ~/eq_assets/EQ_Files/qcat.s3d"]
    fn load_real_zone_has_meshes() {
        let path = PathBuf::from("~/eq_assets/EQ_Files/qcat.s3d");
        if !path.exists() {
            return;
        }
        let assets = ZoneAssets::load(&path).expect("load failed");
        assert!(!assets.meshes.is_empty(), "expected at least one mesh");
    }

    #[test]
    #[ignore = "diagnostic: dumps mesh bounds for qcat and qeynos zones"]
    fn dump_zone_bounds() {
        for zone in &["qcat", "qeynos", "qeynos2"] {
            let path = PathBuf::from(format!("~/eq_assets/EQ_Files/{}.s3d", zone));
            if !path.exists() { continue; }
            let assets = ZoneAssets::load(&path).expect("load failed");
            println!("\n=== {} ({} meshes, {} textures) ===", zone, assets.meshes.len(), assets.textures.len());
            let (mut xmin, mut xmax) = (f32::MAX, f32::MIN);
            let (mut ymin, mut ymax) = (f32::MAX, f32::MIN);
            let (mut zmin, mut zmax) = (f32::MAX, f32::MIN);
            let mut total_verts = 0usize;
            let mut total_tris = 0usize;
            // Also track world bounds (local + center)
            let (mut wxmin, mut wxmax) = (f32::MAX, f32::MIN);
            let (mut wymin, mut wymax) = (f32::MAX, f32::MIN);
            let (mut wzmin, mut wzmax) = (f32::MAX, f32::MIN);
            for m in &assets.meshes {
                total_verts += m.positions.len();
                total_tris += m.indices.len() / 3;
                for &[x, y, z] in &m.positions {
                    xmin = xmin.min(x); xmax = xmax.max(x);
                    ymin = ymin.min(y); ymax = ymax.max(y);
                    zmin = zmin.min(z); zmax = zmax.max(z);
                    wxmin = wxmin.min(x + m.center[0]); wxmax = wxmax.max(x + m.center[0]);
                    wymin = wymin.min(y + m.center[1]); wymax = wymax.max(y + m.center[1]);
                    wzmin = wzmin.min(z + m.center[2]); wzmax = wzmax.max(z + m.center[2]);
                }
            }
            println!("  total verts={} tris={}", total_verts, total_tris);
            println!("  local X: {:.1}..{:.1}  Y: {:.1}..{:.1}  Z: {:.1}..{:.1}",
                xmin, xmax, ymin, ymax, zmin, zmax);
            println!("  world X: {:.1}..{:.1}  Y: {:.1}..{:.1}  Z: {:.1}..{:.1}",
                wxmin, wxmax, wymin, wymax, wzmin, wzmax);
            println!("  world center: ({:.1}, {:.1}, {:.1})",
                (wxmin+wxmax)/2.0, (wymin+wymax)/2.0, (wzmin+wzmax)/2.0);
            // Print a sample mesh center to see if centers are non-zero
            if let Some(m) = assets.meshes.first() {
                println!("  first mesh center: [{:.1}, {:.1}, {:.1}]",
                    m.center[0], m.center[1], m.center[2]);
            }
            if let Some(t) = assets.textures.first() {
                println!("  first texture: {} ({}x{})", t.name, t.width, t.height);
            }
        }
    }

    #[test]
    #[ignore = "requires real extracted assets"]
    fn collision_floor_z_returns_terrain_height() {
        let path = PathBuf::from("~/eq_assets/EQ_Files/qeynos2.s3d");
        if !path.exists() { return; }
        let assets = ZoneAssets::load(&path).expect("load failed");
        let col = Collision::build(&assets, 32.0);

        // Player at qeynos2 waypoint: east=90, north=175 — floor sits around -21..-33.
        let h = col.floor_z(90.0, 175.0, 0.0);
        assert!(h < 0.0 && h > -50.0, "unexpected terrain height: {}", h);
    }

    /// A single horizontal floor quad + one vertical wall: the floor raycast must
    /// return the floor height (not the wall), and a ray crossing the wall must hit.
    #[test]
    fn collision_grid_floor_and_occlusion() {
        // Floor quad at z=0 spanning east/north [0,10]; libeq pos = [east, height, north].
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [10.0, 0.0, 0.0], [10.0, 0.0, 10.0], [0.0, 0.0, 10.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
        };
        // Vertical wall at east=5, spanning north [0,10], height [0,10].
        let wall = MeshData {
            positions: vec![[5.0, 0.0, 0.0], [5.0, 0.0, 10.0], [5.0, 10.0, 10.0], [5.0, 10.0, 0.0]],
            normals: vec![[1.0, 0.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
        };
        let assets = ZoneAssets { meshes: vec![floor, wall], textures: vec![] };
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
        let empty = Collision::build(&ZoneAssets { meshes: vec![], textures: vec![] }, 8.0);
        assert_eq!(empty.floor_z(0.0, 0.0, -99.0), -99.0);
        assert!(!empty.segment_blocked([0.0, 0.0, 0.0], [10.0, 0.0, 0.0]));
        assert!(empty.path_clear([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], 2.0),
            "no geometry should never block movement");
    }

    /// Movement collision: walking toward the wall at east=5 is blocked; walking
    /// parallel to it (along north) or away from it is clear.
    #[test]
    fn collision_path_clear_blocks_walking_into_wall() {
        // Reuse a single vertical wall at east=5, north [0,10], height [0,10].
        let wall = MeshData {
            positions: vec![[5.0, 0.0, 0.0], [5.0, 0.0, 10.0], [5.0, 10.0, 10.0], [5.0, 10.0, 0.0]],
            normals: vec![[1.0, 0.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0, 0.0, 0.0],
        };
        let col = Collision::build(&ZoneAssets { meshes: vec![wall], textures: vec![] }, 4.0);
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
}
