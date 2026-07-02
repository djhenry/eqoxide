//! Zone + texture asset loading and spatial queries.
//!
//! Loads EQ zone GLB/PNG assets into CPU-side `MeshData`/`TextureData`, instances
//! placed objects (buildings, etc.) from the zone's ActorInstance fragments, and indexes equipment
//! textures. Also builds the `Collision` grid and its queries — `floor_z` (grounding),
//! `nearest_hit_t`/`segment_blocked` (camera + nameplate occlusion), `path_clear` (movement
//! gating), and `find_path` (A* routing around walls). See `docs/zone-rendering.md` and
//! `docs/collision-system.md`.

use anyhow::Context;

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
    /// EQ coords (no axis change needed).
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

    /// Load a door/object GLB into model-local meshes keyed by UPPERCASE glTF mesh name,
    /// plus decoded textures. Placement is applied by the caller from live door state.
    pub fn object_models_from_glb(
        path: &std::path::Path,
    ) -> anyhow::Result<(std::collections::HashMap<String, Vec<MeshData>>, Vec<TextureData>)> {
        let g = gltf::Gltf::open(path)
            .with_context(|| format!("open door glb: {}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| std::path::Path::new("./"));
        let buffers = gltf::import_buffers(&g.document, Some(base), g.blob)?;
        let raw_images = gltf::import_images(&g.document, Some(base), &buffers)?;
        let doc = &g.document;

        let mut textures: Vec<TextureData> = Vec::new();
        for (i, image) in doc.images().enumerate() {
            let name = image.name().unwrap_or("").to_string();
            let Some(raw) = raw_images.get(i) else { continue };
            let rgba = match raw.format {
                gltf::image::Format::R8G8B8A8 => raw.pixels.clone(),
                gltf::image::Format::R8G8B8 => raw.pixels.chunks(3)
                    .flat_map(|c| [c[0], c[1], c[2], 255]).collect(),
                _ => continue,
            };
            textures.push(TextureData { name, width: raw.width, height: raw.height, rgba });
        }
        let tex_index_to_name: Vec<String> = doc.textures().map(|t| {
            doc.images().nth(t.source().index()).and_then(|im| im.name().map(|n| n.to_string())).unwrap_or_default()
        }).collect();

        let mut models: std::collections::HashMap<String, Vec<MeshData>> = std::collections::HashMap::new();
        for mesh in doc.meshes() {
            let Some(name) = mesh.name() else { continue };
            let key = name.to_uppercase();
            let mut out: Vec<MeshData> = Vec::new();
            for primitive in mesh.primitives() {
                let reader = primitive.reader(|b| Some(&buffers[b.index()]));
                let Some(positions) = reader.read_positions().map(|i| i.collect::<Vec<[f32;3]>>()) else { continue };
                if positions.is_empty() { continue; }
                let normals = reader.read_normals().map(|i| i.collect())
                    .unwrap_or_else(|| vec![[0.0,0.0,1.0]; positions.len()]);
                let uvs = reader.read_tex_coords(0).map(|t| t.into_f32().collect())
                    .unwrap_or_else(|| vec![[0.0,0.0]; positions.len()]);
                let indices = reader.read_indices().map(|i| i.into_u32().collect())
                    .unwrap_or_else(|| (0..positions.len() as u32).collect());
                let texture_name = primitive.material().pbr_metallic_roughness().base_color_texture()
                    .and_then(|info| tex_index_to_name.get(info.texture().index()).cloned())
                    .filter(|n| !n.is_empty());
                let base_color = primitive.material().pbr_metallic_roughness().base_color_factor();
                out.push(MeshData {
                    positions, normals, uvs, indices, texture_name, base_color,
                    center: [0.0,0.0,0.0], render_mode: RenderMode::Opaque, anim: None,
                });
            }
            if !out.is_empty() { models.entry(key).or_default().extend(out); }
        }
        Ok((models, textures))
    }

    /// Compute the 2D bounding box of all mesh vertices.
    /// Returns `([min_east, min_north], [max_east, max_north])` in EQ world coords
    /// (east = server_x, north = server_y).
    /// EQ WLD position layout: [east, up, north] = [server_x, server_z, server_y].
    pub fn bounds_xy(&self) -> Option<([f32; 2], [f32; 2])> {
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        let expanded = expand_objects(&self.objects);
        for m in self.terrain.iter().chain(expanded.iter()) {
            for p in &m.positions {
                let e = p[2] + m.center[2]; // render.X = server_x = EQ p[2]
                let n = p[0] + m.center[0]; // render.Y = server_y = EQ p[0]
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

/// A swept-collision hit: fraction `t ∈ [0,1]` along the query delta where contact occurs,
/// plus the unit surface normal of the hit triangle (flipped to oppose the motion so callers
/// can project/slide against it). Returned by [`Collision::sweep`].
#[derive(Debug, Clone, Copy)]
pub struct Hit {
    pub t:      f32,
    pub normal: [f32; 3],
}

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
                // EQ WLD -> world: render.X = server_x = p[2], render.Y = server_y = p[0], up = p[1]
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

    // ───────────────────────── Component A: additive movement queries ─────────────────────────
    // These operate generically on `tris` and never touch `build`/the struct fields, so they work
    // whether or not Component B has enriched the triangle set (INVIS faces). See design §5.

    /// True when zone geometry is loaded (the broad-phase grid is non-empty). Callers use this to
    /// skip collision/depenetration entirely when no zone mesh is present.
    pub fn has_geometry(&self) -> bool { self.cols != 0 }

    /// Like [`nearest_hit_t`] but also returns the hit triangle's **unit normal**, flipped to
    /// oppose the segment direction (so it faces back toward `from`). Used by [`sweep`] to provide
    /// the slide plane for collide-and-slide. Möller–Trumbore over the broad-phase cells.
    pub fn nearest_hit(&self, from: [f32; 3], to: [f32; 3]) -> Option<(f32, [f32; 3])> {
        if self.cols == 0 { return None; }
        let dir = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        if dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2] < 1e-9 { return None; }
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
                    if u < 0.0 || u > 1.0 { continue; }
                    let q = cross(tvec, e1);
                    let v = dot(dir, q) * inv;
                    if v < 0.0 || u + v > 1.0 { continue; }
                    let t = dot(e2, q) * inv;
                    if t > 1e-3 && t <= 1.0 && best.map_or(true, |(b, _)| t < b) {
                        // Geometric normal e1×e2, normalised, flipped to face back toward `from`.
                        let mut n = cross(e1, e2);
                        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                        if len > 1e-9 { n = [n[0] / len, n[1] / len, n[2] / len]; }
                        if dot(n, dir) > 0.0 { n = [-n[0], -n[1], -n[2]]; }
                        best = Some((t, n));
                    }
                }
            }
        }
        best
    }

    /// Swept "cylinder" of `radius` moving from `from` by `delta`, approximated by casting the
    /// centre segment plus offset feeler segments at ±radius perpendicular to the horizontal motion
    /// and at foot/chest heights. Returns the nearest contact (fraction `t` + slide-plane normal),
    /// or `None` when the path is clear. (Design §3.1.)
    pub fn sweep(&self, from: [f32; 3], delta: [f32; 3], radius: f32) -> Option<Hit> {
        if self.cols == 0 { return None; }
        let hlen = (delta[0] * delta[0] + delta[1] * delta[1]).sqrt();
        // Perpendicular (in the horizontal plane) to the motion, scaled to radius.
        let perp = if hlen > 1e-6 {
            [-delta[1] / hlen * radius, delta[0] / hlen * radius]
        } else {
            [0.0, 0.0]
        };
        // Foot and chest height offsets (cylinder ~6 units tall, origin at the feet).
        const FOOT: f32 = 0.5;
        const CHEST: f32 = 4.0;
        let mut best: Option<Hit> = None;
        for &(ox, oy) in &[(0.0_f32, 0.0_f32), (perp[0], perp[1]), (-perp[0], -perp[1])] {
            for &hz in &[FOOT, CHEST] {
                let f = [from[0] + ox, from[1] + oy, from[2] + hz];
                let to = [f[0] + delta[0], f[1] + delta[1], f[2] + delta[2]];
                if let Some((t, n)) = self.nearest_hit(f, to) {
                    if best.map_or(true, |b| t < b.t) {
                        best = Some(Hit { t, normal: n });
                    }
                }
            }
        }
        best
    }

    /// Cast a vertical ray from `origin_z` straight down `depth` units at `(east, north)` and
    /// return the height of the nearest surface below, or `None` if nothing is within reach.
    /// Native ground clamp uses `origin = foot_z + 1.0`, `depth = 200` (design §3.2).
    pub fn ground_below(&self, east: f32, north: f32, origin_z: f32, depth: f32) -> Option<f32> {
        if self.cols == 0 { return None; }
        let from = [east, north, origin_z];
        let to   = [east, north, origin_z - depth.max(0.0)];
        self.nearest_hit_t(from, to).map(|t| origin_z - t * depth.max(0.0))
    }

    /// Is the player's cylindrical footprint at `(east, north, foot_z)` clear of geometry?
    /// Samples a horizontal ring of `n` directions at `radius` (and the centre) at chest height,
    /// returning `true` only when none are blocked. Used by the depenetration net (design §3.3).
    pub fn footprint_clear(&self, east: f32, north: f32, foot_z: f32, radius: f32, n: usize) -> bool {
        if self.cols == 0 { return true; }
        let chest = foot_z + 3.0;
        let c = [east, north, chest];
        let n = n.max(1);
        for i in 0..n {
            let a = (i as f32) / (n as f32) * std::f32::consts::TAU;
            let to = [east + a.cos() * radius, north + a.sin() * radius, chest];
            if self.nearest_hit_t(c, to).is_some() { return false; }
        }
        true
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
    /// `avoid` is a set of XY points (nearby NPC positions) the route should skirt — see the
    /// aggro-avoidance note below (#67). Pass `&[]` for a pure geometric route.
    pub fn find_path(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]]) -> Option<Vec<[f32; 3]>> {
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
        // The goal's TIER: the walkable surface at the goal XY nearest the requested goal z. On a
        // zone with stacked levels (neriakc, a walkway over a lower floor) the goal cell exists at
        // several heights; A* must finish on the one the caller asked for, else it routes the whole
        // approach along the wrong tier and the walker stalls / lands a level off (#35). A generous
        // ±STEP_UP band resolves the tier even if goal z is a little off the exact floor.
        let goal_floor = self.nearest_floor(goal[0], goal[1], goal[2], STEP_UP, STEP_UP)
            .unwrap_or(goal[2]);
        const GOAL_TIER_TOL: f32 = 8.0; // reached floor within this of goal_floor == the right tier
        // Start floor: anchor to the caller's EXACT (x,y), NOT the 8u cell center. Near a wall the
        // cell center can fall on the wall's footprint, whose only surface is the wall-TOP — so the
        // center probe would start the char up on the wall and route the whole path along it, a
        // height the walker can't scale from a standstill (it wedges → 0 progress, #2). The exact
        // start point sits on the real floor (e.g. the street) the char is actually standing on.
        // The caller's z can still be stale, so try several reference levels; fall back to the cell
        // center only if the exact point has no floor at any of them.
        let start_floor = [start[2], goal[2], 0.0, -60.0, -120.0]
            .into_iter()
            .find_map(|rz| self.nearest_floor(start[0], start[1], rz, STEP_UP, MAX_DROP))
            .or_else(|| [start[2], goal[2], 0.0, -60.0, -120.0].into_iter().find_map(|rz| floor_near(sc, sr, rz)))
            .unwrap_or(start[2]);
        const STEP_H: f32 = 20.0;        // max CLIMB between adjacent cells (stairs/ledge)
        const MAX_STEP_DOWN: f32 = 60.0; // max DROP between adjacent cells (fall/hop down a level)
        // Snap the start CELL onto the surface the char is really on. When the char stands at a
        // cell's edge next to a wall, the 8u cell CENTER can fall on the wall's footprint — whose
        // only floor is the wall-TOP — so A* run from that cell would begin up on the wall and
        // either route along it (a height the walker can't scale → 0 progress) or find no route at
        // all. If the start cell's column has no floor at the char's true height (`start_floor`),
        // hop to the nearest neighbouring cell that does. (#2)
        let cell_has_start_floor = |c: i32, r: i32| -> bool {
            let ctr = center(c, r);
            self.column_floors(ctr[0], ctr[1], start_floor, STEP_H, MAX_STEP_DOWN)
                .into_iter().any(|z| (z - start_floor).abs() <= GOAL_TIER_TOL)
        };
        let (sc, sr) = if cell_has_start_floor(sc, sr) {
            (sc, sr)
        } else {
            let mut best: Option<(i32, i32, i32)> = None; // (col, row, dist²)
            for rad in 1i32..=3 {
                for dc in -rad..=rad {
                    for dr in -rad..=rad {
                        if dc.abs() != rad && dr.abs() != rad { continue; } // ring only
                        let (nc, nr) = (sc + dc, sr + dr);
                        if nc < 0 || nr < 0 || nc >= cols || nr >= rows { continue; }
                        if cell_has_start_floor(nc, nr) {
                            let d2 = dc * dc + dr * dr;
                            if best.map_or(true, |(_, _, bd)| d2 < bd) { best = Some((nc, nr, d2)); }
                        }
                    }
                }
                if best.is_some() { break; } // nearest ring wins
            }
            best.map(|(c, r, _)| (c, r)).unwrap_or((sc, sr))
        };
        const CHEST: f32 = 3.0;
        const MAX_NODES: usize = 200_000;
        // Aggro-avoidance (#67): softly bias A* AWAY from cells near NPCs so long routes skirt mob
        // camps instead of plowing through them and getting the player killed. Proactive (before
        // aggro) and faction-agnostic — the client has no broad faction data, so it avoids ALL
        // nearby NPCs; the penalty is MILD and fades to 0 at AGGRO_RADIUS, so a route is only
        // nudged around a camp when a clear alternative exists — it never becomes "no route".
        const AGGRO_RADIUS: f32 = 50.0;  // ~ a low-level mob's aggro range
        const AGGRO_PENALTY: f32 = 60.0; // max extra step cost right at an NPC; 0 at the radius edge
        let aggro_cost = |x: f32, y: f32| -> f32 {
            let mut worst = 0.0f32;
            for p in avoid {
                let d2 = (x - p[0]) * (x - p[0]) + (y - p[1]) * (y - p[1]);
                if d2 < AGGRO_RADIUS * AGGRO_RADIUS {
                    worst = worst.max(AGGRO_PENALTY * (1.0 - d2.sqrt() / AGGRO_RADIUS));
                }
            }
            worst
        };
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
        // A goal-cell node reached at the WRONG tier — kept as a last resort so we never regress to
        // "no route" when the requested tier is unreachable (better a wrong-tier path than none).
        let mut goal_fallback: Option<Key> = None;
        while let Some(Node { c, r, fz, .. }) = heap.pop() {
            let ckey = (c, r, qf(fz));
            if !closed.insert(ckey) { continue; } // already expanded
            if (c, r) == (gc, gr) {
                if (fz - goal_floor).abs() <= GOAL_TIER_TOL {
                    goal_key = Some(ckey); // reached the goal cell on the requested tier — done
                    break;
                }
                // Wrong tier: remember the first (cheapest) one, but keep searching — the right tier
                // may be reachable by climbing to it at an adjacent cell. Fall through and expand.
                if goal_fallback.is_none() { goal_fallback = Some(ckey); }
            }
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
                    let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (nf - cz).abs() * 0.5
                        + aggro_cost(b[0], b[1]);
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
        // Prefer the requested tier; fall back to a wrong-tier goal only if the right tier is
        // unreachable (keeps the old "reach the goal cell at all" behaviour as a floor).
        let goal_key = match goal_key.or(goal_fallback) {
            Some(k) => k,
            None => {
                tracing::info!("find_path: no route (expanded={}, cap={}, start_floor={}, goal_floor={})",
                    expanded, MAX_NODES, start_floor, goal_floor);
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
mod door_glb_tests {
    use super::*;
    #[test]
    #[ignore = "requires a baked <zone>_doors.glb at $DOORS_GLB"]
    fn loads_named_door_models() {
        let p = std::env::var("DOORS_GLB").expect("set DOORS_GLB");
        let (models, textures) = ZoneAssets::object_models_from_glb(std::path::Path::new(&p)).unwrap();
        assert!(!models.is_empty(), "expected named door models");
        assert!(models.keys().all(|k| k == &k.to_uppercase()), "keys are uppercase base names");
        let tex: std::collections::HashSet<_> = textures.iter().map(|t| t.name.clone()).collect();
        let linked = models.values().flatten()
            .filter(|m| m.texture_name.as_ref().map_or(false, |n| tex.contains(&n.to_lowercase()))).count();
        assert!(linked > 0, "some door meshes resolve textures by name");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = col.find_path([5.0, 5.0, 0.0], [15.0, 5.0, 0.0], 1.0, &[])
            .expect("a route around the wall should exist");
        let last = *path.last().unwrap();
        assert!((last[0] - 15.0).abs() < 1.5 && (last[1] - 5.0).abs() < 1.5, "ends at goal: {last:?}");
        assert!(path.iter().any(|p| p[1] > 12.0), "path must detour north through the gap: {path:?}");
    }

    #[test]
    fn find_path_skirts_npc_camps_when_given_avoid_points() {
        // Big open floor (no walls) so routing is driven purely by the aggro cost (#67).
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 16.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0];
        // An NPC sitting dead-centre on the straight route.
        let npc = [[100.0, 100.0f32]];
        let min_to_npc = |path: &[[f32; 3]]| path.iter()
            .map(|w| ((w[0] - npc[0][0]).powi(2) + (w[1] - npc[0][1]).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);

        let direct = col.find_path(start, goal, 1.0, &[]).expect("open route exists");
        let skirt  = col.find_path(start, goal, 1.0, &npc).expect("aggro route still exists (mild penalty)");

        // The plain route runs right past the NPC; the aggro route bows away from it.
        assert!(min_to_npc(&direct) < 10.0, "plain route passes through the NPC: {}", min_to_npc(&direct));
        assert!(min_to_npc(&skirt) > min_to_npc(&direct) + 8.0,
            "aggro route should skirt the NPC (min dist {} vs {})", min_to_npc(&skirt), min_to_npc(&direct));
        // …and still arrive.
        let last = *skirt.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 3.0 && (last[1] - goal[1]).abs() < 3.0, "reaches goal: {last:?}");
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

    #[test]
    fn sweep_into_wall_returns_hit_with_facing_normal() {
        let col = Collision::build(
            &ZoneAssets { terrain: vec![wall_east(5.0, 0.0, 10.0)], objects: vec![], textures: vec![] }, 4.0);
        // Moving +east from east=3 by 5 units crosses the wall at east=5.
        let hit = col.sweep([3.0, 0.0, 0.0], [5.0, 0.0, 0.0], 1.0).expect("should hit the wall");
        assert!(hit.t > 0.0 && hit.t < 1.0, "t in (0,1): {}", hit.t);
        // The wall plane is perpendicular to east; the normal must point back toward -east (the
        // side the mover came from), i.e. normal.east < 0.
        assert!(hit.normal[0] < -0.5, "normal should oppose +east motion: {:?}", hit.normal);
        // Moving parallel to the wall (north) from east=3 never reaches it.
        assert!(col.sweep([3.0, 0.0, 0.0], [0.0, 5.0, 0.0], 1.0).is_none(),
            "parallel motion should not hit the wall");
    }

    /// Deterministic offline reproduction of the qeynos2 path-following stalls reported on #2,
    /// using the REAL baked collision mesh. Point `ZONE_GLB` at the cached qeynos2 glb, e.g.
    /// `ZONE_GLB=~/.local/share/eqoxide/assets/models/qeynos2.glb cargo test --lib diagnose_qeynos2_stall -- --ignored --nocapture`
    #[test]
    #[ignore = "requires the cached qeynos2 glb at $ZONE_GLB"]
    fn diagnose_qeynos2_stall() {
        let p = std::env::var("ZONE_GLB").expect("set ZONE_GLB to the cached qeynos2 glb");
        let za = ZoneAssets::from_glb(std::path::Path::new(&p)).unwrap();
        let col = Collision::build(&za, 32.0);
        eprintln!("collision: from_collision_mesh={} grid {}x{} cell={} origin={:?}",
            col.from_collision_mesh, col.cols, col.rows, col.cell_size, col.origin);

        let probe = |label: &str, start: [f32; 3], goal: [f32; 3]| {
            let sf = col.nearest_floor(start[0], start[1], start[2], 20.0, 100.0);
            let gf = col.nearest_floor(goal[0], goal[1], goal[2], 20.0, 100.0);
            eprintln!("\n[{label}] start={start:?} floor={sf:?}  goal={goal:?} floor={gf:?}");
            // What find_path sees at the start CELL CENTER (8u nav grid) vs the exact start point —
            // if these differ, quantization snaps the char onto adjacent (elevated) geometry.
            const NAV_CELL: f32 = 8.0;
            let sc = (((start[0] - col.origin[0]) / NAV_CELL) as i32) as f32;
            let sr = (((start[1] - col.origin[1]) / NAV_CELL) as i32) as f32;
            let ccx = col.origin[0] + (sc + 0.5) * NAV_CELL;
            let ccy = col.origin[1] + (sr + 0.5) * NAV_CELL;
            eprintln!("  start cell center=({ccx:.1},{ccy:.1}) floor@refz={:?}  column={:?}",
                col.nearest_floor(ccx, ccy, start[2], 20.0, 100.0),
                col.column_floors(ccx, ccy, start[2], 20.0, 100.0));
            match col.find_path(start, goal, crate::movement::PLAYER_RADIUS, &[]) {
                Some(path) => {
                    eprintln!("  find_path: {} waypoints", path.len());
                    for (i, w) in path.iter().enumerate().take(6) {
                        eprintln!("    [{i}] ({:.1},{:.1},{:.1})", w[0], w[1], w[2]);
                    }
                    if path.len() > 6 { eprintln!("    ... last ({:.1},{:.1},{:.1})",
                        path.last().unwrap()[0], path.last().unwrap()[1], path.last().unwrap()[2]); }
                }
                None => eprintln!("  find_path: NONE (no route)"),
            }
        };

        // Case A — street-level corner wedge (Kessen). Both reported goals.
        probe("A1 corner-wedge 145u south", [256.4, 324.9, 0.0], [254.0, 180.0, 0.0]);
        probe("A2 corner-wedge 28u NE",     [256.4, 324.9, 0.0], [276.0, 305.0, 0.0]);
        // Case B — multi-level water→street climb (Slink), out of the moat up onto the street.
        probe("B water->street climb",      [-502.3, -141.3, -16.0], [-600.0, -141.0, -5.0]);
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


