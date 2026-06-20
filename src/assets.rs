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
    /// Returns `([min_east, min_north], [max_east, max_north])` in EQ world coords
    /// (east = server_x, north = server_y).
    /// libeq_wld position layout: [east, up, north] = [server_x, server_z, server_y].
    pub fn bounds_xy(&self) -> Option<([f32; 2], [f32; 2])> {
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        for m in &self.meshes {
            for p in &m.positions {
                let e = p[2] + m.center[2]; // render.X = server_x = libeq p[2]
                let n = p[0] + m.center[0]; // render.Y = server_y = libeq p[0]
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
                // libeq -> world: render.X = server_x = p[2], render.Y = server_y = p[0], up = p[1]
                tris.push([
                    [pos[ia][2] + m.center[2], pos[ia][0] + m.center[0], pos[ia][1] + m.center[1]],
                    [pos[ib][2] + m.center[2], pos[ib][0] + m.center[0], pos[ib][1] + m.center[1]],
                    [pos[ic][2] + m.center[2], pos[ic][0] + m.center[0], pos[ic][1] + m.center[1]],
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
    /// Loads ALL .wld files in the archive (e.g. `qeytoqrg.wld`, `objects.wld`,
    /// `lights.wld`) so buildings, trees, and light meshes are included.
    /// Skips unrecognised fragments with a warning instead of returning Err.
    pub fn load(s3d_path: &Path) -> Result<Self> {
        let file = std::fs::File::open(s3d_path)
            .with_context(|| format!("failed to open S3D archive: {}", s3d_path.display()))?;
        let mut pfs = libeq_pfs::PfsReader::open(file)
            .with_context(|| format!("failed to parse PFS archive: {}", s3d_path.display()))?;

        let filenames: Vec<String> = pfs
            .filenames()
            .with_context(|| "failed to list archive filenames")?;

        // Find all .wld files in the archive.
        let wld_files: Vec<&str> = filenames.iter()
            .filter(|f| f.to_lowercase().ends_with(".wld"))
            .map(|f| f.as_str())
            .collect();

        if wld_files.is_empty() {
            anyhow::bail!("no .wld files found in {}", s3d_path.display());
        }

        let mut meshes = Vec::new();
        for wld_name in &wld_files {
            let wld_bytes = match pfs.get(wld_name) {
                Ok(Some(b)) => b,
                Ok(None) => {
                    eprintln!("warning: {} listed but not found in archive", wld_name);
                    continue;
                }
                Err(e) => {
                    eprintln!("warning: failed to read {}: {}", wld_name, e);
                    continue;
                }
            };

            let wld = match libeq_wld::load(&wld_bytes) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("warning: failed to parse {}: {}", wld_name, e);
                    continue;
                }
            };

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
        }

        // Load BMP and DDS textures from the archive.
        let mut textures = Vec::new();
        for filename in &filenames {
            let lower = filename.to_lowercase();
            let fmt = if lower.ends_with(".bmp") {
                image::ImageFormat::Bmp
            } else if lower.ends_with(".dds") {
                image::ImageFormat::Dds
            } else {
                continue;
            };
            let tex_bytes = match pfs.get(filename) {
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

            match image::load_from_memory_with_format(&tex_bytes, fmt) {
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

        eprintln!("zone_assets: loaded {} meshes, {} textures from {} ({} wld files)",
                  meshes.len(), textures.len(), s3d_path.display(), wld_files.len());
        Ok(ZoneAssets { meshes, textures })
    }

    /// Load the base zone S3D plus the `_obj` S3D archive and merge all
    /// meshes and textures.  The `_obj` archive contains buildings, trees,
    /// campfires, signs and other static props that are placed in the zone.
    pub fn load_all(s3d_path: &Path) -> Result<Self> {
        let mut assets = Self::load(s3d_path)?;

        // Try loading the companion _obj.s3d (same directory).
        let stem = s3d_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        let parent = s3d_path.parent().unwrap_or(Path::new("."));
        let obj_path = parent.join(format!("{}_obj.s3d", stem));
        if obj_path.exists() {
            // Textures from the _obj archive (placed objects reference these by name).
            match Self::load(&obj_path) {
                Ok(obj) => {
                    let existing: std::collections::HashSet<String> =
                        assets.textures.iter().map(|t| t.name.clone()).collect();
                    for tex in obj.textures {
                        if !existing.contains(&tex.name) {
                            assets.textures.push(tex);
                        }
                    }
                }
                Err(e) => eprintln!("zone_assets: failed to load textures from {}: {}", obj_path.display(), e),
            }
            // Place object models using the ActorInstance placements in the main zone .wld.
            // (Previously object meshes were merged at their origin → everything piled at 0,0,0.)
            match load_placed_objects(s3d_path, &obj_path) {
                Ok(placed) => assets.meshes.extend(placed),
                Err(e) => eprintln!("zone_assets: object placement failed for {}: {}", obj_path.display(), e),
            }
        }

        Ok(assets)
    }
}

/// Base object name shared by a model's mesh (`NAME_DMSPRITEDEF`) and its placement's
/// ActorDef reference (`NAME_ACTORDEF`), used to match the two.
fn object_base_name(n: &str) -> String {
    let u = n.to_uppercase();
    for suf in ["_DMSPRITEDEF", "_ACTORDEF", "_DMSPRITE", "_DEF"] {
        if let Some(s) = u.strip_suffix(suf) {
            return s.to_string();
        }
    }
    u
}

/// Load placeable object models from `<zone>_obj.s3d` and place each instance using the
/// ActorInstance placements (`WldDoc::objects()`) in the main zone `.wld`. Returns the
/// placed object meshes in libeq space ([east, height, north]) — the same convention as
/// terrain meshes — so `upload_zone_assets` renders them correctly.
fn load_placed_objects(main_s3d: &Path, obj_s3d: &Path) -> Result<Vec<MeshData>> {
    // 1. Object models from _obj.s3d, keyed by base name (vertices include mesh.center).
    let obj_file = std::fs::File::open(obj_s3d)
        .with_context(|| format!("open {}", obj_s3d.display()))?;
    let mut obj_pfs = libeq_pfs::PfsReader::open(obj_file)?;
    let obj_names: Vec<String> = obj_pfs.filenames()?;
    let mut models: std::collections::HashMap<String, Vec<MeshData>> = std::collections::HashMap::new();
    for wn in obj_names.iter().filter(|f| f.to_lowercase().ends_with(".wld")) {
        let bytes = match obj_pfs.get(wn) { Ok(Some(b)) => b, _ => continue };
        let wld = match libeq_wld::load(&bytes) { Ok(w) => w, Err(_) => continue };
        for mesh in wld.meshes() {
            let base = match mesh.name() { Some(n) => object_base_name(n), None => continue };
            let all_pos = mesh.positions();
            if all_pos.is_empty() { continue; }
            let (cx, cy, cz) = mesh.center();
            let all_nrm = mesh.normals();
            let all_uv = mesh.texture_coordinates();
            for prim in mesh.primitives() {
                let idx: Vec<u32> = prim.indices();
                if idx.is_empty() { continue; }
                let positions: Vec<[f32; 3]> = idx.iter()
                    .map(|&i| { let p = all_pos[i as usize]; [p[0] + cx, p[1] + cy, p[2] + cz] })
                    .collect();
                let normals: Vec<[f32; 3]> = idx.iter()
                    .map(|&i| all_nrm.get(i as usize).copied().unwrap_or([0.0, 0.0, 1.0]))
                    .collect();
                let uvs: Vec<[f32; 2]> = idx.iter()
                    .map(|&i| all_uv.get(i as usize).copied().unwrap_or([0.0, 0.0]))
                    .collect();
                let texture_name = prim.material().base_color_texture().and_then(|t| t.source());
                models.entry(base.clone()).or_default().push(MeshData {
                    positions, normals, uvs,
                    indices: (0..idx.len() as u32).collect(),
                    texture_name, base_color: [1.0, 1.0, 1.0, 1.0], center: [0.0, 0.0, 0.0],
                });
            }
        }
    }

    // 2. Placements from the main zone .wld(s): for each, transform the model's meshes.
    let main_file = std::fs::File::open(main_s3d)
        .with_context(|| format!("open {}", main_s3d.display()))?;
    let mut main_pfs = libeq_pfs::PfsReader::open(main_file)?;
    let main_names: Vec<String> = main_pfs.filenames()?;
    let mut placed: Vec<MeshData> = Vec::new();
    let (mut count, mut matched) = (0u32, 0u32);
    for wn in main_names.iter().filter(|f| f.to_lowercase().ends_with(".wld")) {
        let bytes = match main_pfs.get(wn) { Ok(Some(b)) => b, _ => continue };
        let wld = match libeq_wld::load(&bytes) { Ok(w) => w, Err(_) => continue };
        for obj in wld.objects() {
            count += 1;
            let base = match obj.model_name() { Some(n) => object_base_name(n), None => continue };
            let Some(model_meshes) = models.get(&base) else { continue };
            matched += 1;
            // center() = libeq [east, height, north]; rotation() degrees (rz = heading about up);
            // scale() = (x/z scale, y scale). Rotate about the up axis (libeq Y = index 1).
            let (px, py, pz) = obj.center();
            let (_rx, rz, _ry) = obj.rotation();
            let (s_xz, s_y) = obj.scale();
            let scale = if s_y > 0.01 { s_y } else if s_xz > 0.01 { s_xz } else { 1.0 };
            let (sin, cos) = rz.to_radians().sin_cos();
            for m in model_meshes {
                let positions: Vec<[f32; 3]> = m.positions.iter().map(|v| {
                    let (x, y, z) = (v[0] * scale, v[1] * scale, v[2] * scale);
                    [x * cos + z * sin + px, y + py, -x * sin + z * cos + pz]
                }).collect();
                let normals: Vec<[f32; 3]> = m.normals.iter()
                    .map(|n| [n[0] * cos + n[2] * sin, n[1], -n[0] * sin + n[2] * cos])
                    .collect();
                placed.push(MeshData {
                    positions, normals, uvs: m.uvs.clone(), indices: m.indices.clone(),
                    texture_name: m.texture_name.clone(), base_color: m.base_color, center: [0.0, 0.0, 0.0],
                });
            }
        }
    }
    eprintln!("zone_assets: placed {} object meshes ({}/{} placements matched, {} models) from {}",
              placed.len(), matched, count, models.len(), obj_s3d.display());
    Ok(placed)
}

/// Index every BMP/DDS texture filename in an S3D archive to its path (lowercase keys).
/// No decoding — cheap startup scan. Errors are logged and ignored.
pub fn index_s3d_textures(
    s3d_path: &Path,
    out: &mut std::collections::HashMap<String, std::path::PathBuf>,
) {
    let file = match std::fs::File::open(s3d_path) {
        Ok(f) => f,
        Err(e) => { eprintln!("equip: open {} failed: {}", s3d_path.display(), e); return; }
    };
    let mut pfs = match libeq_pfs::PfsReader::open(file) {
        Ok(p) => p,
        Err(e) => { eprintln!("equip: pfs {} failed: {}", s3d_path.display(), e); return; }
    };
    let names = match pfs.filenames() {
        Ok(n) => n,
        Err(e) => { eprintln!("equip: filenames {} failed: {}", s3d_path.display(), e); return; }
    };
    for name in names {
        let lower = name.to_lowercase();
        if lower.ends_with(".bmp") || lower.ends_with(".dds") {
            out.entry(lower).or_insert_with(|| s3d_path.to_path_buf());
        }
    }
}

/// Read and decode a single named texture from an S3D archive.
pub fn load_one_texture_from_s3d(s3d_path: &Path, filename: &str) -> Option<TextureData> {
    let file = std::fs::File::open(s3d_path).ok()?;
    let mut pfs = libeq_pfs::PfsReader::open(file).ok()?;
    let lower = filename.to_lowercase();
    let fmt = if lower.ends_with(".bmp") {
        image::ImageFormat::Bmp
    } else if lower.ends_with(".dds") {
        image::ImageFormat::Dds
    } else {
        return None;
    };
    // PFS lookups are case-sensitive; find the real archive name case-insensitively.
    let names = pfs.filenames().ok()?;
    let real = names.into_iter().find(|n| n.to_lowercase() == lower)?;
    let bytes = pfs.get(&real).ok()??;
    let img = image::load_from_memory_with_format(&bytes, fmt).ok()?;
    let rgba = img.to_rgba8();
    let (width, height) = rgba.dimensions();
    Some(TextureData { name: lower, width, height, rgba: rgba.into_raw() })
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
        // Vertical wall at world east=5: libeq p2=5 (render.X), spanning north=p0 [0,10], height=p1 [0,10].
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
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

    #[test]
    #[ignore = "requires ~/eq_assets/EQ_Files/global17_amr.s3d"]
    fn index_and_load_one_armor_texture() {
        use std::collections::HashMap;
        let p = std::path::PathBuf::from(
            format!("{}/eq_assets/EQ_Files/global17_amr.s3d", std::env::var("HOME").unwrap()));
        let mut idx: HashMap<String, std::path::PathBuf> = HashMap::new();
        index_s3d_textures(&p, &mut idx);
        assert!(idx.contains_key("homch1701.bmp"), "expected human male chest armor 17");
        let tex = load_one_texture_from_s3d(idx.get("homch1701.bmp").unwrap(), "homch1701.bmp");
        let tex = tex.expect("decode failed");
        assert!(tex.width > 0 && tex.height > 0);
        assert_eq!(tex.rgba.len(), (tex.width * tex.height * 4) as usize);
    }

    /// Movement collision: walking toward the wall at east=5 is blocked; walking
    /// parallel to it (along north) or away from it is clear.
    #[test]
    fn collision_path_clear_blocks_walking_into_wall() {
        // Vertical wall at world east=5: libeq p2=5 (render.X), north=p0 [0,10], height=p1 [0,10].
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 5.0], [10.0, 0.0, 5.0], [10.0, 10.0, 5.0], [0.0, 10.0, 5.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
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


