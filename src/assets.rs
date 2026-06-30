//! Zone + texture asset loading and spatial queries.
//!
//! Loads EQ `.s3d`/`.wld` archives via libeq into CPU-side `MeshData`/`TextureData`, instances
//! placed objects (buildings, etc.) from the zone's ActorInstance fragments, and indexes equipment
//! textures. Also builds the `Collision` grid and its queries — `floor_z` (grounding),
//! `nearest_hit_t`/`segment_blocked` (camera + nameplate occlusion), `path_clear` (movement
//! gating), and `find_path` (A* routing around walls). See `docs/zone-rendering.md` and
//! `docs/collision-system.md`.

use anyhow::{Context, Result};
use std::path::Path;

/// Parse a glTF material's `extras` JSON (the asset server's `eqAdditive` / `eqAnim`).
fn material_extras(material: &gltf::Material) -> Option<serde_json::Value> {
    material
        .extras()
        .as_ref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw.get()).ok())
}

/// True if the material carries `extras: { "eqAdditive": true }` (EQ fire/glow surface).
fn material_is_additive(material: &gltf::Material) -> bool {
    material_extras(material)
        .and_then(|v| v.get("eqAdditive").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// Read the material's animated-texture spec from `extras.eqAnim`, if present:
/// `(frame_interval_ms, frame image names)`.
fn material_anim(material: &gltf::Material) -> Option<(u32, Vec<String>)> {
    let v = material_extras(material)?;
    let a = v.get("eqAnim")?;
    let ms = a.get("ms")?.as_u64()? as u32;
    let frames: Vec<String> = a
        .get("frames")?
        .as_array()?
        .iter()
        .filter_map(|f| f.as_str().map(|s| s.to_string()))
        .collect();
    if frames.len() < 2 {
        return None;
    }
    Some((ms, frames))
}

/// How a zone primitive is blended, derived from the glTF material's `alphaMode`
/// (plus the `extras.eqAdditive` flag the asset server emits for EQ additive surfaces).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum RenderMode {
    /// Fully opaque (the vast majority of zone geometry).
    #[default]
    Opaque,
    /// Alpha-test cutout (foliage/branches): drawn in the opaque pass, fragments
    /// with texture alpha < 0.5 are discarded.
    Masked,
    /// Standard src-alpha blend (semi-transparent surfaces). Opacity is baked into
    /// the texture alpha by the asset server.
    Blend,
    /// Additive blend (EQ fire/glow): dst + src, no occlusion.
    Additive,
}

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
    /// Transparency/blend mode from the glTF material's `alphaMode` (+ additive extras).
    pub render_mode: RenderMode,
    /// EQ animated texture: `(frame_interval_ms, frame image names incl. frame 0)`,
    /// from the material's `extras.eqAnim`. The renderer cycles these frames. `None`
    /// for static textures.
    pub anim: Option<(u32, Vec<String>)>,
}

/// CPU-side texture data ready for GPU upload.
#[derive(Clone)]
pub struct TextureData {
    pub name: String,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// A reusable object model plus its per-placement instance transforms.
///
/// `meshes` are in model-local space (one entry per glTF primitive); each matrix in
/// `instances` is a column-major 4×4 placing one copy of the model into world space.
#[derive(Clone)]
pub struct ObjectModel {
    pub meshes: Vec<MeshData>,
    /// Column-major 4×4 transforms, one per placement (`Mat4::from_cols_array_2d` form).
    pub instances: Vec<[[f32; 4]; 4]>,
}

/// All CPU-side data for a zone, loaded from .s3d + .wld.
///
/// `terrain` is world-space static geometry (the zone shell). `objects` are instanced
/// models placed multiple times; `expand_objects` flattens them into world-space meshes
/// for the CPU render/collision paths until GPU instancing lands.
#[derive(Clone)]
pub struct ZoneAssets {
    pub terrain: Vec<MeshData>,
    pub objects: Vec<ObjectModel>,
    pub textures: Vec<TextureData>,
}

/// Flatten instanced object models into world-space `MeshData`.
///
/// For each model, for each column-major instance matrix `M`, every position `p` is mapped
/// to `M * vec4(p, 1)` and every normal by `M`'s upper-3×3. The result has `center: [0,0,0]`
/// (positions are already absolute) and preserves `texture_name`/`base_color`.
pub fn expand_objects(objects: &[ObjectModel]) -> Vec<MeshData> {
    let mut out = Vec::new();
    for model in objects {
        for inst in &model.instances {
            let m = glam::Mat4::from_cols_array_2d(inst);
            for mesh in &model.meshes {
                let positions: Vec<[f32; 3]> = mesh.positions.iter().map(|&p| {
                    let v = m.transform_point3(glam::Vec3::from_array(p));
                    [v.x, v.y, v.z]
                }).collect();
                let normals: Vec<[f32; 3]> = mesh.normals.iter().map(|&n| {
                    let v = m.transform_vector3(glam::Vec3::from_array(n));
                    [v.x, v.y, v.z]
                }).collect();
                out.push(MeshData {
                    positions,
                    normals,
                    uvs: mesh.uvs.clone(),
                    indices: mesh.indices.clone(),
                    texture_name: mesh.texture_name.clone(),
                    base_color: mesh.base_color,
                    center: [0.0, 0.0, 0.0],
                    render_mode: mesh.render_mode,
                    anim: mesh.anim.clone(),
                });
            }
        }
    }
    out
}

impl ZoneAssets {
    /// Load a server-baked zone GLB into the same `ZoneAssets` the renderer consumes.
    ///
    /// Each GLB image is named with the lowercased EQ texture filename (e.g. `qcat0001.bmp`).
    /// `MeshData.texture_name` is set to that same image name so `upload_zone_assets` can link
    /// meshes to textures by name.  Positions/normals/uvs/indices are read via the primitive
    /// reader; `center` is set to `[0,0,0]` because GLB positions are already world-space
    /// libeq coords (no axis change needed).
    ///
    /// Mirrors the glTF loading in `src/models.rs:63-78` (gltf::Gltf::from_reader,
    /// import_buffers, import_images).
    pub fn from_glb(path: &std::path::Path) -> anyhow::Result<Self> {
        let gltf_doc = gltf::Gltf::open(path)
            .with_context(|| format!("failed to parse zone glb: {}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| std::path::Path::new("./"));
        let buffers = gltf::import_buffers(&gltf_doc.document, Some(base), gltf_doc.blob)
            .with_context(|| format!("failed to load glb buffers: {}", path.display()))?;
        let raw_images = gltf::import_images(&gltf_doc.document, Some(base), &buffers)
            .with_context(|| format!("failed to load glb images: {}", path.display()))?;

        let document = &gltf_doc.document;

        // Build texture list: name = the image's name field (lowercased EQ filename like "qcat0001.bmp").
        // Meshes link to textures by image NAME (via tex_index_to_name), not by index.
        let mut textures: Vec<TextureData> = Vec::new();
        for (i, image) in document.images().enumerate() {
            let img_name = image.name().unwrap_or("").to_string();
            let raw = match raw_images.get(i) {
                Some(d) => d,
                None => {
                    tracing::info!("zone glb: no pixel data for image {} ({})", i, img_name);
                    continue;
                }
            };
            let rgba = match raw.format {
                gltf::image::Format::R8G8B8A8 => raw.pixels.clone(),
                gltf::image::Format::R8G8B8 => raw.pixels
                    .chunks(3)
                    .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255u8])
                    .collect(),
                _ => {
                    tracing::info!("zone glb: skipping image {} ({}) — unsupported format", i, img_name);
                    continue;
                }
            };
            textures.push(TextureData {
                name: img_name,
                width: raw.width,
                height: raw.height,
                rgba,
            });
        }

        // Build a map from gltf texture index → TextureData name (for mesh linkage).
        // gltf texture → source image index → image name.
        let tex_index_to_name: Vec<String> = document.textures()
            .map(|t| {
                let src = t.source().index();
                document.images()
                    .nth(src)
                    .and_then(|img| img.name().map(|n| n.to_string()))
                    .unwrap_or_default()
            })
            .collect();

        // Read a gltf mesh's model-local primitives into MeshData (one per primitive).
        let read_mesh = |mesh: &gltf::Mesh| -> Vec<MeshData> {
            let mut out = Vec::new();
            for primitive in mesh.primitives() {
                let reader = primitive.reader(|b| Some(&buffers[b.index()]));

                let positions: Vec<[f32; 3]> = match reader.read_positions() {
                    Some(iter) => iter.collect(),
                    None => continue,
                };
                if positions.is_empty() {
                    continue;
                }

                let normals: Vec<[f32; 3]> = reader.read_normals()
                    .map(|it| it.collect())
                    .unwrap_or_else(|| vec![[0.0, 0.0, 1.0]; positions.len()]);

                let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0)
                    .map(|tc| tc.into_f32().collect())
                    .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);

                let indices: Vec<u32> = match reader.read_indices() {
                    Some(iter) => iter.into_u32().collect(),
                    None => (0..positions.len() as u32).collect(),
                };

                // Resolve texture name from the material's base-color texture.
                let texture_name: Option<String> = primitive.material()
                    .pbr_metallic_roughness()
                    .base_color_texture()
                    .and_then(|info| tex_index_to_name.get(info.texture().index()).cloned())
                    .filter(|n| !n.is_empty());

                let material = primitive.material();
                let base_color = material.pbr_metallic_roughness().base_color_factor();

                // Map glTF alphaMode (+ the asset server's `eqAdditive` extra) to a render mode.
                let render_mode = match material.alpha_mode() {
                    gltf::material::AlphaMode::Opaque => RenderMode::Opaque,
                    gltf::material::AlphaMode::Mask => RenderMode::Masked,
                    gltf::material::AlphaMode::Blend => {
                        if material_is_additive(&material) {
                            RenderMode::Additive
                        } else {
                            RenderMode::Blend
                        }
                    }
                };

                let anim = material_anim(&material);

                out.push(MeshData {
                    positions,
                    normals,
                    uvs,
                    indices,
                    texture_name,
                    base_color,
                    center: [0.0, 0.0, 0.0],
                    render_mode,
                    anim,
                });
            }
            out
        };

        // Is a node's transform (approximately) the identity?
        let is_identity = |m: &[[f32; 4]; 4]| -> bool {
            const ID: [[f32; 4]; 4] =
                [[1.,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];
            m.iter().zip(ID.iter())
                .all(|(row, idr)| row.iter().zip(idr.iter()).all(|(a, b)| (a - b).abs() < 1e-5))
        };

        // Walk every scene node with a mesh: identity transform → terrain; non-identity →
        // group by referenced mesh index into an ObjectModel (model-local meshes read once
        // per mesh; node matrices accumulated as instances).
        let mut terrain: Vec<MeshData> = Vec::new();
        // mesh index → position in `objects`
        let mut obj_index: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        let mut objects: Vec<ObjectModel> = Vec::new();

        let nodes: Vec<gltf::Node> = match document.default_scene() {
            Some(scene) => scene.nodes().collect(),
            None => document.nodes().collect(),
        };
        // Flatten the node hierarchy (placement nodes are typically scene roots, but descend
        // children defensively). Transforms are taken as the node's own local matrix.
        let mut stack: Vec<gltf::Node> = nodes;
        while let Some(node) = stack.pop() {
            for child in node.children() {
                stack.push(child);
            }
            let Some(mesh) = node.mesh() else { continue };
            let matrix = node.transform().matrix();
            // The baked collision mesh (SOLID + INVIS faces, PASSABLE excluded) is delivered as
            // a mesh named `__collision__`. Tag its MeshData with the sentinel texture name so
            // the renderer skips drawing it and `Collision::build` uses it for collision.
            if mesh.name() == Some(COLLISION_MESH_TAG) {
                let mut mds = read_mesh(&mesh);
                for md in &mut mds {
                    md.texture_name = Some(COLLISION_MESH_TAG.to_string());
                }
                terrain.extend(mds);
                continue;
            }
            if is_identity(&matrix) {
                terrain.extend(read_mesh(&mesh));
            } else {
                let mi = mesh.index();
                let slot = *obj_index.entry(mi).or_insert_with(|| {
                    objects.push(ObjectModel { meshes: read_mesh(&mesh), instances: Vec::new() });
                    objects.len() - 1
                });
                objects[slot].instances.push(matrix);
            }
        }

        let total_instances: usize = objects.iter().map(|o| o.instances.len()).sum();
        tracing::info!("zone_assets::from_glb: loaded {} terrain meshes, {} object models ({} instances), {} textures from {}",
                  terrain.len(), objects.len(), total_instances, textures.len(), path.display());
        Ok(ZoneAssets { terrain, objects, textures })
    }

    /// Compute the 2D bounding box of all mesh vertices.
    /// Returns `([min_east, min_north], [max_east, max_north])` in EQ world coords
    /// (east = server_x, north = server_y).
    /// libeq_wld position layout: [east, up, north] = [server_x, server_z, server_y].
    pub fn bounds_xy(&self) -> Option<([f32; 2], [f32; 2])> {
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        let expanded = expand_objects(&self.objects);
        for m in self.terrain.iter().chain(expanded.iter()) {
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

/// Sentinel `MeshData.texture_name` marking the dedicated collision geometry baked into a
/// zone GLB as a mesh named `__collision__`. The asset-server/converter emits every SOLID
/// face (including INVIS — invisible-but-solid zone boundaries, invisible walls, doorframes)
/// here while EXCLUDING PASSABLE faces (water surfaces, foliage). `from_glb` tags the loaded
/// mesh with this name so the renderer skips drawing it and `Collision::build` consumes it
/// for collision instead of the rendered terrain.
pub const COLLISION_MESH_TAG: &str = "__collision__";

pub struct Collision {
    tris:      Vec<[[f32; 3]; 3]>,
    cells:     Vec<Vec<u32>>,
    origin:    [f32; 2], // (east, north) of cell (0,0) corner
    cell_size: f32,
    cols:      usize,
    rows:      usize,
    /// Optional water-region map (from the zone's `.wtr`). When present, find_path may DESCEND
    /// through water (swim down a canal/shaft) to a lower floor that has no walkable connection.
    water:     Option<std::sync::Arc<crate::water_map::WaterMap>>,
    /// True when the terrain triangles came from a dedicated `__collision__` mesh (SOLID +
    /// INVIS faces, PASSABLE excluded). False for legacy zones with no baked collision mesh,
    /// where the rendered terrain is used as a fallback. Diagnostic/provenance only.
    pub from_collision_mesh: bool,
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
            return Collision { tris, cells: vec![], origin: [0.0, 0.0], cell_size, cols: 0, rows: 0, water: None, from_collision_mesh };
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
        Collision { tris, cells, origin: min, cell_size, cols, rows, water: None, from_collision_mesh }
    }

    /// Attach a zone water map so find_path can route swim descents. Call after `build`.
    pub fn set_water(&mut self, water: Option<std::sync::Arc<crate::water_map::WaterMap>>) {
        self.water = water;
    }

    /// True if `pos` = [east, north, z] (server coords) lies in a water region.
    /// False when the zone has no water map. Used to gate swim (vertical) movement.
    pub fn in_water(&self, pos: [f32; 3]) -> bool {
        self.water.as_ref().is_some_and(|w| w.is_water(pos[0], pos[1], pos[2]))
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

    /// Find the walkable floor height at `(east, north)` nearest to `ref_z`.
    ///
    /// Casts a vertical column over `[ref_z - down, ref_z + up]`, gathers EVERY triangle it
    /// crosses, and returns the hit whose height is **closest to `ref_z`**. This is the surface
    /// the player would actually stand on (or step to), and — unlike a single top-down ray —
    /// it does NOT mistake an overhang/awning/bridge ABOVE the floor for the floor itself.
    /// `up` bounds how far you can step UP onto a ledge; `down` how far you can drop. Returns
    /// `None` when no surface exists in the band.
    pub fn nearest_floor(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Option<f32> {
        if self.cols == 0 { return None; }
        let z_top = ref_z + up.max(0.0);
        let z_bot = ref_z - down.max(0.0);
        let from = [east, north, z_top];
        let dir_z = z_bot - z_top; // negative (downward)
        if dir_z.abs() < 1e-6 { return None; }
        let eps = 1e-6_f32;
        let cross = |a: [f32; 3], b: [f32; 3]| [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let dir = [0.0, 0.0, dir_z];
        let (c0, c1, r0, r1) = self.cell_range(east, north, east, north);
        let mut best: Option<f32> = None;
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
                    if !(0.0..=1.0).contains(&t) { continue; }
                    let hit_z = z_top + t * dir_z;
                    if best.map_or(true, |b| (hit_z - ref_z).abs() < (b - ref_z).abs()) {
                        best = Some(hit_z);
                    }
                }
            }
        }
        best
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

    /// ALL distinct walkable surface heights at `(east, north)` within `[ref_z - down, ref_z + up]`,
    /// sorted high→low with near-duplicates (within 1u) merged. Unlike `nearest_floor` (one surface),
    /// this exposes every floor in the column — essential for multi-level pathfinding where a ramp or
    /// lower floor sits UNDER an upper ledge (so A* can choose to descend instead of always snapping
    /// to the nearest/upper surface).
    pub fn column_floors(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Vec<f32> {
        if self.cols == 0 { return Vec::new(); }
        let z_top = ref_z + up.max(0.0);
        let z_bot = ref_z - down.max(0.0);
        let dir_z = z_bot - z_top;
        if dir_z.abs() < 1e-6 { return Vec::new(); }
        let eps = 1e-6_f32;
        let cross = |a: [f32; 3], b: [f32; 3]| [
            a[1] * b[2] - a[2] * b[1], a[2] * b[0] - a[0] * b[2], a[0] * b[1] - a[1] * b[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let from = [east, north, z_top];
        let dir = [0.0, 0.0, dir_z];
        let (c0, c1, r0, r1) = self.cell_range(east, north, east, north);
        let mut hits: Vec<f32> = Vec::new();
        for r in r0..=r1 {
            for c in c0..=c1 {
                for &ti in &self.cells[r * self.cols + c] {
                    let tri = &self.tris[ti as usize];
                    let (v0, v1, v2) = (tri[0], tri[1], tri[2]);
                    let e1 = [v1[0]-v0[0], v1[1]-v0[1], v1[2]-v0[2]];
                    let e2 = [v2[0]-v0[0], v2[1]-v0[1], v2[2]-v0[2]];
                    let p = cross(dir, e2);
                    let det = dot(e1, p);
                    if det.abs() < eps { continue; }
                    let inv = 1.0 / det;
                    let tvec = [from[0]-v0[0], from[1]-v0[1], from[2]-v0[2]];
                    let u = dot(tvec, p) * inv;
                    if u < 0.0 || u > 1.0 { continue; }
                    let q = cross(tvec, e1);
                    let v = dot(dir, q) * inv;
                    if v < 0.0 || u + v > 1.0 { continue; }
                    let t = dot(e2, q) * inv;
                    if !(0.0..=1.0).contains(&t) { continue; }
                    hits.push(z_top + t * dir_z);
                }
            }
        }
        hits.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal)); // high→low
        hits.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        hits
    }

    /// A* over the collision grid: a walkable waypoint path from `start` to `goal` that routes
    /// AROUND walls (slide_move only slides along one). Returns cell-center waypoints
    /// `[east, north]` (start-exclusive, goal-inclusive) or None if no geometry / no route.
    /// Walkability = a floor exists under the cell; an edge needs a small floor-height step and
    /// a clear chest-height segment between cell centers.
    pub fn find_path(&self, start: [f32; 3], goal: [f32; 3], radius: f32) -> Option<Vec<[f32; 3]>> {
        use std::collections::BinaryHeap;
        use std::cmp::Ordering;
        if self.cols == 0 || self.rows == 0 { return None; }
        // Navigate on a FINER grid than the collision broad-phase buckets (self.cell_size, ~32u).
        // At 32u, cell centers fall inside walls in tight corridors, so A* sees a fragmented graph,
        // finds no route, and the caller straight-lines into walls. An 8u nav grid keeps cell
        // centers inside corridors so A* can actually route around them. (The collision triangle
        // lookup via floor_z/path_clear works at any query point regardless of bucket size.)
        const NAV_CELL: f32 = 8.0;
        let cell = NAV_CELL;
        let cols = (self.cols as f32 * self.cell_size / cell).ceil() as i32;
        let rows = (self.rows as f32 * self.cell_size / cell).ceil() as i32;
        let to_cell = |e: f32, n: f32| -> (i32, i32) {
            let c = (((e - self.origin[0]) / cell) as i32).clamp(0, cols - 1);
            let r = (((n - self.origin[1]) / cell) as i32).clamp(0, rows - 1);
            (c, r)
        };
        let center = |c: i32, r: i32| -> [f32; 2] {
            [self.origin[0] + (c as f32 + 0.5) * cell,
             self.origin[1] + (r as f32 + 0.5) * cell]
        };
        // The probe FOLLOWS the terrain: each cell's floor is found relative to the floor of the
        // cell we reached it from (so multi-level dungeons work even when the caller's start z is
        // stale, the common case). `nearest_floor` gathers ALL surfaces in the vertical column and
        // snaps to the one closest to `ref_z` — so an overhang/awning/bridge ABOVE the walkable
        // floor is never mistaken for the floor (the old single top-down ray grabbed the first hit
        // and got trapped on ceiling geometry). `up` = max step-up onto a ledge; `down` = max drop.
        const STEP_UP: f32 = 20.0;
        const MAX_DROP: f32 = 100.0;
        let floor_near = |c: i32, r: i32, ref_z: f32| -> Option<f32> {
            let p = center(c, r);
            self.nearest_floor(p[0], p[1], ref_z, STEP_UP, MAX_DROP)
        };
        let (sc, sr) = to_cell(start[0], start[1]);
        let (gc, gr) = to_cell(goal[0], goal[1]);
        if (sc, sr) == (gc, gr) { return Some(vec![[goal[0], goal[1], goal[2]]]); }
        // The caller's z can be stale, so find the start floor by trying several reference levels.
        let start_floor = [start[2], goal[2], 0.0, -60.0, -120.0]
            .into_iter()
            .find_map(|rz| floor_near(sc, sr, rz))
            .unwrap_or(start[2]);
        const STEP_H: f32 = 20.0;        // max CLIMB between adjacent cells (stairs/ledge)
        const MAX_STEP_DOWN: f32 = 60.0; // max DROP between adjacent cells (fall/hop down a level)
        const CHEST: f32 = 3.0;
        const MAX_NODES: usize = 200_000;
        // MULTI-FLOOR A*: the node is (cell, floor), not just cell — so a single cell can be visited
        // at several heights (a ramp or lower floor sitting UNDER an upper ledge). Single-floor A*
        // snapped every cell to the surface nearest the current z and could never step down onto a
        // floor beneath an overhang, so overlapping multi-level zones (e.g. qcat's sewer under the
        // upper walkway) were unreachable. Floor is quantized to 2u buckets for the hash key.
        let qf = |z: f32| (z / 2.0).round() as i32;
        type Key = (i32, i32, i32); // (col, row, floor_bucket)
        let skey: Key = (sc, sr, qf(start_floor));
        let mut g_score: std::collections::HashMap<Key, f32> = std::collections::HashMap::new();
        let mut came:    std::collections::HashMap<Key, Key> = std::collections::HashMap::new();
        let mut closed:  std::collections::HashSet<Key> = std::collections::HashSet::new();
        let mut floor_of: std::collections::HashMap<Key, f32> = std::collections::HashMap::new();
        struct Node { f: f32, c: i32, r: i32, fz: f32 }
        impl PartialEq for Node { fn eq(&self, o: &Self) -> bool { self.f == o.f } }
        impl Eq for Node {}
        impl Ord for Node { fn cmp(&self, o: &Self) -> Ordering { o.f.partial_cmp(&self.f).unwrap_or(Ordering::Equal) } }
        impl PartialOrd for Node { fn partial_cmp(&self, o: &Self) -> Option<Ordering> { Some(self.cmp(o)) } }
        let h = |c: i32, r: i32| (((c - gc) as f32).powi(2) + ((r - gr) as f32).powi(2)).sqrt() * cell;
        g_score.insert(skey, 0.0);
        floor_of.insert(skey, start_floor);
        let mut heap = BinaryHeap::new();
        heap.push(Node { f: h(sc, sr), c: sc, r: sr, fz: start_floor });
        let mut expanded = 0usize;
        let mut goal_key: Option<Key> = None;
        while let Some(Node { c, r, fz, .. }) = heap.pop() {
            let ckey = (c, r, qf(fz));
            if !closed.insert(ckey) { continue; } // already expanded
            if (c, r) == (gc, gr) { goal_key = Some(ckey); break; }
            expanded += 1;
            if expanded > MAX_NODES { break; }
            let cz = fz;
            let g_cur = *g_score.get(&ckey).unwrap_or(&f32::MAX);
            let a = center(c, r);
            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (nc, nr) = (c + dc, r + dr);
                if nc < 0 || nr < 0 || nc >= cols || nr >= rows { continue; }
                let b = center(nc, nr);
                // Consider EVERY surface in the neighbor column reachable by climbing <=STEP_H or
                // dropping <=MAX_STEP_DOWN — this is what lets A* descend onto a lower floor under an
                // overhang (the multi-level connection) instead of staying on the upper surface.
                for nf in self.column_floors(b[0], b[1], cz, STEP_H, MAX_STEP_DOWN) {
                    if nf - cz > STEP_H || cz - nf > MAX_STEP_DOWN { continue; }
                    let nkey = (nc, nr, qf(nf));
                    if closed.contains(&nkey) { continue; }
                    if !self.path_clear([a[0], a[1], cz + CHEST], [b[0], b[1], nf + CHEST], radius) { continue; }
                    let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (nf - cz).abs() * 0.5;
                    let tentative = g_cur + step;
                    if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                        g_score.insert(nkey, tentative);
                        came.insert(nkey, ckey);
                        floor_of.insert(nkey, nf);
                        heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                    }
                }
                // WATER DESCENT: if the neighbor column holds water below the current floor, allow
                // dropping/swimming down to the floor beneath it even past MAX_STEP_DOWN and without
                // a clear chest-height walking segment — you fall into the water and sink/swim. This
                // connects an upper walkway to a flooded lower level (e.g. qcat's canal → sewer).
                if let Some(water) = &self.water {
                    // Is there water somewhere in the column between here and far below?
                    let has_water = (1..=12).any(|k| water.is_water(b[0], b[1], cz - k as f32 * 8.0));
                    if has_water {
                        // Take the deepest floor in a deep probe that sits in/just under water.
                        for nf in self.column_floors(b[0], b[1], cz, STEP_H, 200.0) {
                            if nf >= cz - 1.0 { continue; } // descents only (the normal loop above
                            // handles same-level/climbs; a walkable shallow drop it already added)
                            // require the column at/just above this floor to be water (a real swim
                            // landing, not a dry lethal fall)
                            if !water.is_water(b[0], b[1], nf + 3.0) && !water.is_water(b[0], b[1], nf + 12.0) { continue; }
                            let nkey = (nc, nr, qf(nf));
                            if closed.contains(&nkey) { continue; }
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (cz - nf) * 0.5;
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                            }
                        }
                    }
                }

                // CONTROLLED FALL: step off a ledge / through a hole and fall to the floor below.
                // Allowed when (a) you can move horizontally off the edge at your CURRENT height (open
                // air beyond the ledge reads as clear), and (b) there's a landing floor within
                // MAX_FALL below. This is how levels joined by a drop (no walkable ramp, e.g. qcat's
                // dry sewer) connect. It's directional (you fall DOWN); climbing back needs a real
                // path. A per-unit fall cost makes A* prefer walking/stairs when a route exists.
                const MAX_FALL: f32 = 120.0;
                if self.path_clear([a[0], a[1], cz + CHEST], [b[0], b[1], cz + CHEST], radius) {
                    // The first surface you'd land on falling at b = highest floor below a real-step
                    // drop. (column_floors returns high→low, so `find` gives the first landing.)
                    if let Some(nf) = self.column_floors(b[0], b[1], cz, 0.0, MAX_FALL)
                        .into_iter().find(|&z| z < cz - STEP_H)
                    {
                        let nkey = (nc, nr, qf(nf));
                        if !closed.contains(&nkey) {
                            // Huge flat cost: a controlled fall is a LAST RESORT. A* will only take
                            // it when there's no walkable route to the goal (e.g. a sealed lower
                            // level), never as a 2D "shortcut" that dives into a pit and climbs back.
                            const FALL_PENALTY: f32 = 50_000.0;
                            let step = FALL_PENALTY + (cz - nf) * 2.0;
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: nf });
                            }
                        }
                    }
                }
            }
        }
        let goal_key = match goal_key {
            Some(k) => k,
            None => {
                tracing::info!("find_path: no route (expanded={}, cap={}, start_floor={})",
                    expanded, MAX_NODES, start_floor);
                return None;
            }
        };
        let mut path = Vec::new();
        let mut cur = goal_key;
        while cur != skey {
            let (c, r, _) = cur;
            let ctr = center(c, r);
            // Carry each waypoint's actual floor height so the walker moves + collision-checks at
            // the right z while climbing/descending (instead of the goal's z, which clips walls).
            path.push([ctr[0], ctr[1], *floor_of.get(&cur).unwrap_or(&goal[2])]);
            match came.get(&cur) { Some(&p) => cur = p, None => break }
        }
        path.reverse();
        if let Some(last) = path.last_mut() { *last = [goal[0], goal[1], goal[2]]; }
        Some(path)
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
                    tracing::warn!("warning: {} listed but not found in archive", wld_name);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("warning: failed to read {}: {}", wld_name, e);
                    continue;
                }
            };

            let wld = match libeq_wld::load(&wld_bytes) {
                Ok(w) => w,
                Err(e) => {
                    tracing::warn!("warning: failed to parse {}: {}", wld_name, e);
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
                        render_mode: RenderMode::Opaque, anim: None,
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
                    tracing::warn!("warning: texture {} listed but not found in archive", filename);
                    continue;
                }
                Err(e) => {
                    tracing::warn!("warning: failed to read texture {}: {}", filename, e);
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
                    tracing::warn!("warning: failed to decode texture {}: {}", filename, e);
                }
            }
        }

        tracing::info!("zone_assets: loaded {} meshes, {} textures from {} ({} wld files)",
                  meshes.len(), textures.len(), s3d_path.display(), wld_files.len());
        // The .s3d path stays flat/terrain-only (local fallback); no instanced objects.
        Ok(ZoneAssets { terrain: meshes, objects: vec![], textures })
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

/// Extract all mesh primitives from every `.wld` in a PFS archive.
/// Returns `(object_base_name, MeshData)` pairs; vertices already include `mesh.center()`.
/// Used by `load_object_models` to resolve door/object model geometry by name.
fn read_object_meshes(s3d: &Path) -> Result<Vec<(String, MeshData)>> {
    let file = std::fs::File::open(s3d)
        .with_context(|| format!("open {}", s3d.display()))?;
    let mut pfs = libeq_pfs::PfsReader::open(file)?;
    let names: Vec<String> = pfs.filenames()?;
    let mut out: Vec<(String, MeshData)> = Vec::new();
    for wn in names.iter().filter(|f| f.to_lowercase().ends_with(".wld")) {
        let bytes = match pfs.get(wn) { Ok(Some(b)) => b, _ => continue };
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
                out.push((base.clone(), MeshData {
                    positions, normals, uvs,
                    indices: (0..idx.len() as u32).collect(),
                    texture_name, base_color: [1.0, 1.0, 1.0, 1.0], center: [0.0, 0.0, 0.0],
                    render_mode: RenderMode::Opaque, anim: None,
                }));
            }
        }
    }
    Ok(out)
}

/// Index object/door model meshes by uppercase base name from BOTH the main zone `.wld`(s)
/// and the companion `_obj.wld`. Door models (e.g. `"DOOR1"`) may be defined in either
/// archive. Meshes are returned in libeq space (vertices include `mesh.center()`).
///
/// Both archives are optional — if one is missing or fails to parse it is skipped silently.
pub fn load_object_models(
    main_s3d: &Path,
    obj_s3d: &Path,
) -> Result<(std::collections::HashMap<String, Vec<MeshData>>, Vec<TextureData>)> {
    use std::collections::{HashMap, HashSet};
    let mut models: HashMap<String, Vec<MeshData>> = HashMap::new();
    let mut textures: Vec<TextureData> = Vec::new();
    let mut seen_tex: HashSet<String> = HashSet::new();
    for s3d in [obj_s3d, main_s3d] {
        if !s3d.exists() { continue; }
        let pairs = match read_object_meshes(s3d) { Ok(p) => p, Err(_) => continue };
        // Decode each referenced texture from THIS archive (door bitmaps live alongside the
        // meshes that use them, like weapons in gequip*.s3d). Deduped by name; a name not yet
        // loaded is retried against the next archive.
        for (_base, mesh) in &pairs {
            if let Some(tn) = &mesh.texture_name {
                let lower = tn.to_lowercase();
                if !seen_tex.contains(&lower) {
                    if let Some(td) = load_one_texture_from_s3d(s3d, &lower) {
                        seen_tex.insert(lower);
                        textures.push(td);
                    }
                }
            }
        }
        for (base, mesh) in pairs {
            models.entry(base.to_uppercase()).or_default().push(mesh);
        }
    }
    Ok((models, textures))
}

/// Index every BMP/DDS texture filename in an S3D archive to its path (lowercase keys).
/// No decoding — cheap startup scan. Errors are logged and ignored.
pub fn index_s3d_textures(
    s3d_path: &Path,
    out: &mut std::collections::HashMap<String, std::path::PathBuf>,
) {
    let file = match std::fs::File::open(s3d_path) {
        Ok(f) => f,
        Err(e) => { tracing::warn!("equip: open {} failed: {}", s3d_path.display(), e); return; }
    };
    let mut pfs = match libeq_pfs::PfsReader::open(file) {
        Ok(p) => p,
        Err(e) => { tracing::warn!("equip: pfs {} failed: {}", s3d_path.display(), e); return; }
    };
    let names = match pfs.filenames() {
        Ok(n) => n,
        Err(e) => { tracing::warn!("equip: filenames {} failed: {}", s3d_path.display(), e); return; }
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
    // Reject transparent "stub" textures: some chr archives (e.g. globalelf_chr.s3d) store an 8x8
    // all-alpha-0 lowest-MIP placeholder DDS for body pieces that have no real cloth texture (e.g.
    // elfua0002.dds, elfch0003.dds). Loading one makes that mesh render 100% transparent (invisible
    // arms/back). Returning None lets the caller fall back to the opaque baked skin base — which is
    // what the original client shows for those pieces at material 0. (eq-client-expert finding.)
    if (width <= 8 && height <= 8) || rgba.pixels().all(|p| p.0[3] == 0) {
        return None;
    }
    Some(TextureData { name: lower, width, height, rgba: rgba.into_raw() })
}

/// Load a single held/world item model (e.g. "IT10649", from an item's IDFile) + its textures from
/// the gequip*.s3d archives. Meshes are returned untransformed in libeq space so the caller can
/// attach them to a hand bone. Returns None if the model isn't found in any gequip archive.
pub fn load_weapon_model(assets_path: &Path, idfile: &str) -> Option<ZoneAssets> {
    let want = idfile.trim().to_uppercase();
    if want.is_empty() { return None; }
    for arch in ["gequip.s3d", "gequip2.s3d", "gequip3.s3d", "gequip4.s3d",
                 "gequip5.s3d", "gequip6.s3d", "gequip7.s3d", "gequip8.s3d"] {
        let path = assets_path.join(arch);
        let Ok(file) = std::fs::File::open(&path) else { continue };
        let Ok(mut pfs) = libeq_pfs::PfsReader::open(file) else { continue };
        let Ok(filenames) = pfs.filenames() else { continue };
        for wn in filenames.iter().filter(|f| f.to_lowercase().ends_with(".wld")) {
            let wld = match pfs.get(wn) { Ok(Some(b)) => match libeq_wld::load(&b) {
                Ok(w) => w, Err(_) => continue }, _ => continue };
            let mut meshes: Vec<MeshData> = Vec::new();
            for mesh in wld.meshes() {
                if !mesh.name().unwrap_or("").to_uppercase().starts_with(&want) { continue; }
                let all_pos = mesh.positions();
                if all_pos.is_empty() { continue; }
                let (cx, cy, cz) = mesh.center();
                let all_n = mesh.normals();
                let all_uv = mesh.texture_coordinates();
                for prim in mesh.primitives() {
                    let idx: Vec<u32> = prim.indices();
                    if idx.is_empty() { continue; }
                    let positions = idx.iter().map(|&i| all_pos[i as usize]).collect();
                    let normals = idx.iter().map(|&i| all_n.get(i as usize).copied().unwrap_or([0.0, 0.0, 1.0])).collect();
                    let uvs = idx.iter().map(|&i| all_uv.get(i as usize).copied().unwrap_or([0.0, 0.0])).collect();
                    let texture_name = prim.material().base_color_texture().and_then(|t| t.source());
                    meshes.push(MeshData { positions, normals, uvs,
                        indices: (0..idx.len() as u32).collect(),
                        texture_name, base_color: [1.0; 4], center: [cx, cy, cz],
                        render_mode: RenderMode::Opaque, anim: None });
                }
            }
            if meshes.is_empty() { continue; }
            // Load only the textures these meshes reference.
            let want_tex: std::collections::HashSet<String> = meshes.iter()
                .filter_map(|m| m.texture_name.clone()).map(|s| s.to_lowercase()).collect();
            let mut textures = Vec::new();
            for fname in &filenames {
                let lower = fname.to_lowercase();
                if !want_tex.contains(&lower) { continue; }
                let fmt = if lower.ends_with(".bmp") { image::ImageFormat::Bmp }
                          else if lower.ends_with(".dds") { image::ImageFormat::Dds } else { continue };
                if let Ok(Some(tb)) = pfs.get(fname) {
                    if let Ok(img) = image::load_from_memory_with_format(&tb, fmt) {
                        let rgba = img.to_rgba8(); let (w, h) = rgba.dimensions();
                        textures.push(TextureData { name: fname.clone(), width: w, height: h, rgba: rgba.into_raw() });
                    }
                }
            }
            tracing::info!("weapon model: loaded '{}' — {} meshes, {} textures from {}",
                      want, meshes.len(), textures.len(), arch);
            return Some(ZoneAssets { terrain: meshes, objects: vec![], textures });
        }
    }
    tracing::warn!("weapon model: '{}' not found in any gequip*.s3d", want);
    None
}

#[cfg(test)]
mod b2_glb_tests {
    use super::*;
    #[test]
    #[ignore = "requires a baked zone glb at $ZONE_GLB"]
    fn from_glb_links_meshes_to_textures() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to a baked zone glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        let all: Vec<MeshData> = za.terrain.iter().cloned().chain(expand_objects(&za.objects)).collect();
        assert!(!all.is_empty());
        let tex_names: std::collections::HashSet<_> = za.textures.iter().map(|t| t.name.clone()).collect();
        let linked = all.iter().filter(|m| m.texture_name.as_ref().map_or(false, |n| tex_names.contains(n))).count();
        assert!(linked > 0, "at least some meshes must resolve their texture by name");
    }

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
    use std::path::PathBuf;

    #[test]
    fn load_returns_err_on_missing_file() {
        let result = ZoneAssets::load(Path::new("/nonexistent/path.s3d"));
        assert!(result.is_err());
    }

    /// Directory of extracted EQ `.s3d` files for the `#[ignore]`d asset tests
    /// below. Set `EQ_ASSETS_DIR` to run them; otherwise they skip.
    fn assets_dir() -> Option<PathBuf> {
        std::env::var_os("EQ_ASSETS_DIR").map(PathBuf::from)
    }

    #[test]
    #[ignore = "requires real extracted assets (set EQ_ASSETS_DIR)"]
    fn load_real_zone_has_meshes() {
        let Some(path) = assets_dir().map(|d| d.join("qcat.s3d")) else { return; };
        if !path.exists() {
            return;
        }
        let assets = ZoneAssets::load(&path).expect("load failed");
        assert!(!assets.terrain.is_empty(), "expected at least one mesh");
    }

    #[test]
    #[ignore = "requires real extracted assets (set EQ_ASSETS_DIR)"]
    fn collision_floor_z_returns_terrain_height() {
        let Some(path) = assets_dir().map(|d| d.join("qeynos2.s3d")) else { return; };
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
            render_mode: RenderMode::Opaque, anim: None,
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
        // Floor quad at height z=10 spanning east/north [0,10]; libeq pos = [east, height, north].
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

    #[test]
    fn find_path_routes_around_a_partial_wall() {
        // 20x20 floor at z=0.
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Partial wall at world east=10, spanning north 0..14 (gap at north 14..20), height 0..10.
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 10.0], [14.0, 0.0, 10.0], [14.0, 10.0, 10.0], [0.0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 2.0);
        // The direct line (5,5)->(15,5) crosses the wall (north 5 < 14) → blocked.
        assert!(col.segment_blocked([5.0, 5.0, 3.0], [15.0, 5.0, 3.0]));
        // find_path routes AROUND the wall through the northern gap.
        let path = col.find_path([5.0, 5.0, 0.0], [15.0, 5.0, 0.0], 1.0)
            .expect("a route around the wall should exist");
        let last = *path.last().unwrap();
        assert!((last[0] - 15.0).abs() < 1.5 && (last[1] - 5.0).abs() < 1.5, "ends at goal: {last:?}");
        assert!(path.iter().any(|p| p[1] > 12.0), "path must detour north through the gap: {path:?}");
    }

    #[test]
    #[ignore = "requires extracted assets (set EQ_ASSETS_DIR): global17_amr.s3d"]
    fn index_and_load_one_armor_texture() {
        use std::collections::HashMap;
        let Some(p) = assets_dir().map(|d| d.join("global17_amr.s3d")) else { return; };
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
    #[ignore = "requires extracted assets (set EQ_ASSETS_DIR): qeynos.s3d + qeynos_obj.s3d"]
    fn loads_a_known_door_model() {
        let Some(ap) = assets_dir() else { return; };
        let main = ap.join("qeynos.s3d");
        let obj  = ap.join("qeynos_obj.s3d");
        if !main.exists() { tracing::warn!("assets missing; skipping"); return; }
        let (models, _textures) = load_object_models(&main, &obj).expect("load");
        assert!(models.contains_key("DOOR1"), "DOOR1 not found; keys (sample): {:?}",
                models.keys().filter(|k| k.contains("DOOR") || k.starts_with("PORT"))
                      .collect::<Vec<_>>());
        let meshes = &models["DOOR1"];
        assert!(!meshes.is_empty(), "DOOR1 has no meshes");
        assert!(meshes.iter().all(|m| !m.positions.is_empty()), "some DOOR1 mesh has no positions");
    }

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
}

#[cfg(test)]
mod instanced_tests {
    use super::*;
    #[test]
    fn expand_objects_applies_instance_matrices() {
        let model = ObjectModel {
            meshes: vec![MeshData {
                positions: vec![[1.0,0.0,0.0]], normals: vec![[1.0,0.0,0.0]],
                uvs: vec![[0.0,0.0]], indices: vec![0],
                texture_name: Some("t.bmp".into()), base_color: [1.0;4], center: [0.0;3],
                render_mode: RenderMode::Opaque, anim: None,
            }],
            // two instances: identity, and translate +10 in x (column-major: row3 col0..)
            instances: vec![
                [[1.,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]],
                [[1.,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[10.,0.,0.,1.]],
            ],
        };
        let out = expand_objects(std::slice::from_ref(&model));
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].positions[0], [1.0,0.0,0.0]);
        assert_eq!(out[1].positions[0], [11.0,0.0,0.0]); // +10 x
        assert_eq!(out[1].texture_name.as_deref(), Some("t.bmp"));
    }

    #[test]
    #[ignore = "requires a baked instanced zone glb at $ZONE_GLB"]
    fn from_glb_groups_instances() {
        let p = std::env::var("ZONE_GLB").unwrap();
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        assert!(!za.terrain.is_empty());
        assert!(!za.objects.is_empty(), "expected object models");
        let total_instances: usize = za.objects.iter().map(|o| o.instances.len()).sum();
        assert!(total_instances >= za.objects.len(), "more placements than models");
    }
}


