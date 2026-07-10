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

/// Compact one primitive to only the vertices its indices actually reference, remapping the
/// indices accordingly. glTF meshes frequently share ONE POSITION accessor across many primitives
/// (each primitive is just an index subset — e.g. qeynos's `terrain` is 242 primitives over a
/// single 51,700-vertex pool). gltf-rs's `read_positions()` returns that FULL shared pool for
/// every primitive, so uploading positions as-read duplicates the whole pool once per primitive:
/// qeynos = 242 × 51,700 ≈ 12.5M verts (~400 MB) for 51,700 unique verts. That ~400 MB exhausts
/// GPU memory and the zone terrain fails to render (a void), while a low-primitive zone like
/// ecommons (43 prims ≈ 16 MB) is unaffected. Compaction makes the emitted vertex count track the
/// triangles a primitive actually draws, independent of the shared pool's size. (eqoxide#213)
fn compact_primitive(
    positions: Vec<[f32; 3]>,
    normals:   Vec<[f32; 3]>,
    uvs:       Vec<[f32; 2]>,
    indices:   Vec<u32>,
) -> (Vec<[f32; 3]>, Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>) {
    let mut remap = vec![u32::MAX; positions.len()];
    let mut np = Vec::new();
    let mut nn = Vec::new();
    let mut nu = Vec::new();
    let mut ni = Vec::with_capacity(indices.len());
    for &i in &indices {
        let iu = i as usize;
        if iu >= positions.len() { continue; } // defensive: drop out-of-range (valid GLBs never hit)
        let r = if remap[iu] == u32::MAX {
            let nr = np.len() as u32;
            remap[iu] = nr;
            np.push(positions[iu]);
            nn.push(normals.get(iu).copied().unwrap_or([0.0, 0.0, 1.0]));
            nu.push(uvs.get(iu).copied().unwrap_or([0.0, 0.0]));
            nr
        } else {
            remap[iu]
        };
        ni.push(r);
    }
    (np, nn, nu, ni)
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

                // Drop the shared-vertex-pool overhead: emit only the vertices this primitive
                // references (see compact_primitive — fixes the qeynos 242×-pool blowup). (eqoxide#213)
                let (positions, normals, uvs, indices) =
                    compact_primitive(positions, normals, uvs, indices);
                if positions.is_empty() { continue; }

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
    water:     Option<std::sync::Arc<crate::region_map::RegionMap>>,
    /// True when the terrain triangles came from a dedicated `__collision__` mesh (SOLID +
    /// INVIS faces, PASSABLE excluded). False for legacy zones with no baked collision mesh,
    /// where the rendered terrain is used as a fallback. Diagnostic/provenance only.
    pub from_collision_mesh: bool,
    /// Precomputed `(zone_line_index, [east, north, z])` for each zone-line region, built once from
    /// the water map at `set_water` (zone load). Lets `find_zone_line_near` be an O(1) cache read on
    /// the network thread instead of an exhaustive scan that linkdead-ed the client (#204).
    zone_line_regions: Vec<(i32, [f32; 3])>,
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
            return Collision { tris, cells: vec![], origin: [0.0, 0.0], cell_size, cols: 0, rows: 0, water: None, from_collision_mesh, zone_line_regions: Vec::new() };
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
        Collision { tris, cells, origin: min, cell_size, cols, rows, water: None, from_collision_mesh, zone_line_regions: Vec::new() }
    }

    /// Attach a zone water map so find_path can route swim descents. Call after `build`.
    pub fn set_water(&mut self, water: Option<std::sync::Arc<crate::region_map::RegionMap>>) {
        self.water = water;
        // Precompute zone-line region points now (zone load, off the net thread) so the runtime
        // find_zone_line_near is an O(1) cache read (#204).
        self.zone_line_regions = self.precompute_zone_line_regions();
    }

    /// Build the zone-line region cache from the water map. Bounded + timed; warns if it runs long
    /// (nav prep should never be slow enough to matter next to keepalive). Empty when there's no
    /// water map / no zone-line regions.
    fn precompute_zone_line_regions(&self) -> Vec<(i32, [f32; 3])> {
        let Some(water) = self.water.as_ref() else { return Vec::new(); };
        if self.cols == 0 || self.tris.is_empty() { return Vec::new(); }
        let (mut zmin, mut zmax) = (f32::MAX, f32::MIN);
        for t in &self.tris { for v in t { zmin = zmin.min(v[2]); zmax = zmax.max(v[2]); } }
        let bounds = (
            self.origin[0], self.origin[0] + self.cols as f32 * self.cell_size,
            self.origin[1], self.origin[1] + self.rows as f32 * self.cell_size,
            zmin, zmax,
        );
        let t0 = std::time::Instant::now();
        let regions = water.zone_line_region_points(bounds);
        let ms = t0.elapsed().as_millis();
        if ms > 250 {
            tracing::warn!("nav: zone-line region precompute took {ms}ms ({} region(s)) — slower than expected", regions.len());
        } else if !regions.is_empty() {
            tracing::info!("nav: precomputed {} zone-line region point(s) in {ms}ms", regions.len());
        }
        regions
    }

    /// True if `pos` = [east, north, z] (server coords) lies in a water region.
    /// False when the zone has no water map. Used to gate swim (vertical) movement.
    pub fn in_water(&self, pos: [f32; 3]) -> bool {
        self.water.as_ref().is_some_and(|w| w.is_water(pos[0], pos[1], pos[2]))
    }

    /// Water-surface height above a submerged `pos`, or `None` if not in water / no bounded surface.
    /// Used by the controller's buoyancy to float toward the surface (#172).
    pub fn water_surface(&self, pos: [f32; 3]) -> Option<f32> {
        self.water.as_ref().and_then(|w| w.surface_z(pos[0], pos[1], pos[2]))
    }

    /// If `pos` = [east, north, z] (server coords) lies in a zone-line (`DRNTP`) region, the
    /// zone-point index it carries — the `OP_SendZonepoints` `iterator` for that line, used to
    /// resolve the destination zone. `None` when not on a zone line, no region map, or a v1 map.
    /// This is how the native client triggers a crossing: it detects the region from zone geometry
    /// rather than a coordinate list.
    pub fn zone_line_at(&self, pos: [f32; 3]) -> Option<i32> {
        self.water.as_ref().and_then(|w| w.zone_line_at(pos[0], pos[1], pos[2]))
    }

    /// Distinct zone-point indices of every zone-line region in this zone — the set of exits. Each
    /// links to an entrance via the `OP_SendZonepoints` `iterator`. Empty when the zone has no
    /// region map (or a v1 map with no indices).
    pub fn zone_line_indices(&self) -> Vec<i32> {
        self.water.as_ref().map(|w| w.zone_line_indices()).unwrap_or_default()
    }

    /// Find a point inside a zone-line region nearest to `near` (= [east, north, z]), returning
    /// `(region_index, point)`. When `index` is `Some`, only that zone-point index matches; `None`
    /// matches any zone line. Used by the explicit `/zone_cross` API to walk the character onto the
    /// zone line, where the auto-cross then fires. `None` if the zone has no region map or no
    /// matching region is found within the search radius.
    ///
    /// O(regions) cache read — NO scan. The region points are precomputed once at zone load
    /// (`set_water`), so this is safe to call on the network thread; the old per-request expanding
    /// ring × z scan did up to ~10^8 BSP walks synchronously and force-linkdead-ed the client when a
    /// line was far away or missing from the `.wtr` (#204). `index` filters to a destination zone's
    /// line (`None` = any); returns `(index, [east, north, z])` of the region nearest `near`.
    pub fn find_zone_line_near(&self, index: Option<i32>, near: [f32; 3]) -> Option<(i32, [f32; 3])> {
        self.zone_line_regions
            .iter()
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .min_by(|a, b| {
                let d2 = |p: &[f32; 3]| (p[0] - near[0]).powi(2) + (p[1] - near[1]).powi(2) + (p[2] - near[2]).powi(2);
                d2(&a.1).partial_cmp(&d2(&b.1)).unwrap_or(std::cmp::Ordering::Equal)
            })
            .copied()
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
    /// A* over the walkable-floor grid. `radius` is the clearance the route must keep from geometry
    /// (smaller threads narrower gaps). When `allow_partial` is true and the goal cell is
    /// unreachable, returns a path to the nearest-reachable cell toward the goal instead of `None`
    /// (so a stranded character still makes progress, #188); when false, only a route that reaches
    /// the goal cell is returned. `None` = no progress possible (truly boxed in).
    /// Default nav plan: the standard 8u grid over the WHOLE zone (long-range routing). This is the
    /// coarse tier of the two-tier planner (#nav-multires) — cheap over big distances but blind to
    /// sub-8u detail (thin ramps, narrow openings). The FINE local tier calls `find_path_res` with a
    /// small cell + a search bound to thread that detail near the walker.
    pub fn find_path(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]], allow_partial: bool) -> Option<Vec<[f32; 3]>> {
        self.find_path_res(start, goal, radius, avoid, allow_partial, 8.0, None)
    }

    /// A* at an arbitrary grid resolution `cell`, optionally bounded to `max_search` units of the
    /// start (so a FINE plan stays local + cheap even if it hits an obstacle). `cell` = 8.0 +
    /// `max_search` = None reproduces the classic whole-zone nav grid.
    pub fn find_path_res(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        allow_partial: bool, cell: f32, max_search: Option<f32>) -> Option<Vec<[f32; 3]>> {
        use std::collections::BinaryHeap;
        use std::cmp::Ordering;
        if self.cols == 0 || self.rows == 0 { return None; }
        // Navigate on a FINER grid than the collision broad-phase buckets (self.cell_size, ~32u).
        // At 32u, cell centers fall inside walls in tight corridors, so A* sees a fragmented graph,
        // finds no route, and the caller straight-lines into walls. An 8u nav grid keeps cell
        // centers inside corridors so A* can actually route around them; a finer cell (the local
        // tier) resolves thin ramps/openings. (The collision triangle lookup via floor_z/path_clear
        // works at any query point regardless of bucket size.)
        let cell = cell.max(1.0);
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
        const STEP_H: f32 = 20.0;        // vertical SEARCH range for column_floors + per-cell rise cap
        // What actually enforces "nav climbs only what a WASD player can" (#239) is NOT a per-cell
        // rise cap (that would reject legitimate smooth ramps) — it's the FEET-level `path_clear`
        // below: a discrete riser taller than the walker's ~2.5u step blocks the low ray, so A* routes
        // around it, while a smooth ramp (surface stays under the ray) passes and is governed by
        // MAX_WALK_GRADE. Paired with the controller's native STEP_UP cap (no more NAV_CLIMB=20), nav
        // can no longer scale the boundary-wall lips it used to climb onto the high side of.
        const MAX_STEP_DOWN: f32 = 60.0; // max DROP between adjacent cells (fall/hop down a level)
        // Grade limit (eqoxide#212): STEP_H=20 over an 8u cell is a 250% grade. A discrete vertical
        // step that tall is already blocked here by the chest-ray path_clear (its riser is a wall),
        // so the climbs that actually reach A* are smooth RAMPS — and a ramp steeper than the
        // controller can walk makes it slide on the face and wedge (#205). Reject a climb whose
        // grade (rise/run) exceeds what's walkable; A* then routes around the steep face.
        const MAX_WALK_GRADE: f32 = 1.2;  // walkable up to ~50° (rise/run); steeper = slide
        // Jump-edges (eqoxide#190): let A* leap a GENUINE horizontal floor gap a running jump can
        // clear. NAV_RUN_SPEED matches navigation::RUN_SPEED (the speed the walker drives a jump at);
        // reach is derived from it via movement::running_jump_reach (~22.7u). JUMP_UP_TOL caps how
        // much higher a landing may sit (a running jump's apex ≈ JUMP_VELOCITY²/2·GRAVITY ≈ 4u).
        // JUMP_PENALTY makes a jump cost more than the equivalent walk so A* only jumps when a gap
        // would otherwise block the route.
        const NAV_RUN_SPEED: f32 = 44.0;
        const JUMP_UP_TOL: f32 = 4.0;
        const JUMP_PENALTY: f32 = 30.0;
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
        // Closest-to-goal cell we actually reach, for a partial-path fallback (#188): if the goal
        // cell itself is unreachable, still walk AS FAR toward it as we can rather than not moving at
        // all — the walker's re-path loop then makes further incremental progress from there.
        let mut best_toward: Option<Key> = None;
        let mut best_toward_h = f32::MAX;
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
            // Track the closest-to-goal cell reached (heuristic = straight-line cells to the goal),
            // for the partial-path fallback below.
            let hd = h(c, r);
            if hd < best_toward_h { best_toward_h = hd; best_toward = Some(ckey); }
            let cz = fz;
            let g_cur = *g_score.get(&ckey).unwrap_or(&f32::MAX);
            let a = center(c, r);
            for (dc, dr) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                let (nc, nr) = (c + dc, r + dr);
                if nc < 0 || nr < 0 || nc >= cols || nr >= rows { continue; }
                let b = center(nc, nr);
                // Local-tier bound: keep a FINE plan within `max_search` units of the start so its
                // cost stays small even when it has to detour around an obstacle (#nav-multires).
                if let Some(maxr) = max_search {
                    if (b[0] - start[0]).hypot(b[1] - start[1]) > maxr { continue; }
                }
                // Consider EVERY surface in the neighbor column reachable by climbing <=STEP_H or
                // dropping <=MAX_STEP_DOWN — this is what lets A* descend onto a lower floor under an
                // overhang (the multi-level connection) instead of staying on the upper surface.
                for nf in self.column_floors(b[0], b[1], cz, STEP_H, MAX_STEP_DOWN) {
                    if nf - cz > STEP_H || cz - nf > MAX_STEP_DOWN { continue; }
                    // Grade limit: skip a climb too steep to walk (rise/run > MAX_WALK_GRADE) —
                    // A* then routes around the slope face instead of wedging on it. (eqoxide#212)
                    let rise = nf - cz;
                    if rise > 0.0 {
                        let run = (((dc * dc + dr * dr) as f32).sqrt()) * cell; // 8u orth / ~11.3u diag
                        if rise / run > MAX_WALK_GRADE { continue; }
                    }
                    let nkey = (nc, nr, qf(nf));
                    if closed.contains(&nkey) { continue; }
                    // Reachability rays. The CHEST ray (3u) alone SKIMS OVER a low invisible-boundary
                    // lip (~2–3u) — A* then routes onto the wall's high side, where the feet-level
                    // walker snags and strands (#239). Add a FEET-level ray just above the walker's
                    // real max step-up (STEP_UP + ground-snap ≈ 2.5u): a lip taller than the walker can
                    // mount blocks the edge, matching what the native client's feet-level sphere does.
                    const FEET_CLR: f32 = crate::movement::STEP_UP + 0.5;
                    if !self.path_clear([a[0], a[1], cz + CHEST], [b[0], b[1], nf + CHEST], radius) { continue; }
                    if !self.path_clear([a[0], a[1], cz + FEET_CLR], [b[0], b[1], nf + FEET_CLR], radius) { continue; }
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

                // JUMP-EDGE (eqoxide#190): a running jump crosses a GENUINE horizontal floor gap —
                // wider than one cell (so normal walk edges can't bridge it) but within jump reach.
                // Only fires in a CARDINAL direction whose ADJACENT cell is a gap (no walkable floor
                // to step to — otherwise it's just walking). Land on the nearest cell within reach
                // whose floor is at ~takeoff height or lower, with a clear arc. Costs more than
                // walking (JUMP_PENALTY) so A* prefers real routes and only leaps when a gap blocks it.
                if (dc == 0) != (dr == 0) {
                    let walkable_at = |x: f32, y: f32| {
                        self.column_floors(x, y, cz, STEP_H, MAX_STEP_DOWN)
                            .into_iter().any(|f| f - cz <= STEP_H && cz - f <= MAX_STEP_DOWN)
                    };
                    if !walkable_at(b[0], b[1]) {
                        let reach = crate::movement::running_jump_reach(NAV_RUN_SPEED);
                        let max_k = (reach / cell).floor() as i32;
                        for k in 2..=max_k.max(2) {
                            let (jc, jr) = (c + dc * k, r + dr * k);
                            if jc < 0 || jr < 0 || jc >= cols || jr >= rows { break; }
                            if (k as f32) * cell > reach { break; }
                            // every intermediate cell must be a gap (no walkable floor near cz);
                            // if there's ground between, it's not a real jump gap.
                            let all_gap = (1..k).all(|j| {
                                let m = center(c + dc * j, r + dr * j);
                                !walkable_at(m[0], m[1])
                            });
                            if !all_gap { break; }
                            // landing floor: at ~takeoff height (a jump gains ≤ JUMP_UP_TOL) or lower.
                            let land = center(jc, jr);
                            let landing = self.column_floors(land[0], land[1], cz, JUMP_UP_TOL, MAX_STEP_DOWN)
                                .into_iter()
                                .filter(|&nf| nf - cz <= JUMP_UP_TOL && cz - nf <= MAX_STEP_DOWN)
                                .max_by(|x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal));
                            let Some(nf) = landing else { continue };
                            // arc clear: no wall between takeoff and landing (chest height).
                            if !self.path_clear([a[0], a[1], cz + CHEST], [land[0], land[1], nf + CHEST], radius) {
                                continue;
                            }
                            let nkey = (jc, jr, qf(nf));
                            if closed.contains(&nkey) { continue; }
                            let tentative = g_cur + (k as f32) * cell + JUMP_PENALTY;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, nf);
                                heap.push(Node { f: tentative + h(jc, jr), c: jc, r: jr, fz: nf });
                            }
                            break; // nearest valid landing in this direction wins
                        }
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
                            // Steep per-depth cost so descending to a pool BOTTOM is a last resort:
                            // A* should cross a surface pool at the top (cheap surface-traversal edge
                            // above) and only dive when reaching a genuinely lower level is the only
                            // way (a flooded sewer). Without this bias A* dove straight to the floor
                            // of the Halas pool and the swimmer got stranded there (#191).
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (cz - nf) * 4.0;
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

                // WATER ASCENT: the reverse of the descent — if THIS column is submerged
                // (water above the current floor), the character can swim up to the surface
                // and haul out onto a neighbor floor at or below surface + STEP_H. Without
                // this, flooded pits (qeynos2's moat) are one-way traps: descent gets you in,
                // and the normal climb's chest ray hits the pit wall on the way out.
                if let Some(water) = &self.water {
                    let submerged = (0..=3).any(|k| water.is_water(a[0], a[1], cz + 2.0 + k as f32 * 4.0));
                    if submerged {
                        // Top of the contiguous water column above the current floor.
                        let mut surface = cz;
                        while surface - cz < 200.0 && water.is_water(a[0], a[1], surface + 2.0) {
                            surface += 2.0;
                        }
                        // A swimmer floats at the surface and can only STEP out onto a low lip — the
                        // controller's swim step-up is the native STEP_UP (~2.5u with the ground snap),
                        // NOT a 20u climb. Capping the haul-out here to that keeps A* from routing a
                        // vertical scramble from the water onto a bridge/ledge the walker can't perform
                        // (#nav-multires: the water analogue of the #239 climb limit). A genuinely
                        // walkable exit is a beach/ramp, handled by the normal ground edges, not here.
                        const WATER_EXIT_UP: f32 = crate::movement::STEP_UP + 0.5;
                        for nf in self.column_floors(b[0], b[1], surface, STEP_H, surface - cz) {
                            if nf <= cz + 1.0 { continue; }              // ascents only
                            if nf > surface + WATER_EXIT_UP { continue; } // too high to haul out of water
                            let nkey = (nc, nr, qf(nf));
                            if closed.contains(&nkey) { continue; }
                            // Swim at the surface, then the usual chest clearance for the
                            // step out — the ray starts at swim height, so it passes over
                            // the pit lip that blocks the ground-level climb ray.
                            let ray_z = surface.max(nf - STEP_H);
                            if !self.path_clear([a[0], a[1], ray_z + CHEST], [b[0], b[1], nf + CHEST], radius) { continue; }
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (nf - cz) * 0.5
                                + aggro_cost(b[0], b[1]);
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

                // WATER SURFACE TRAVERSAL: swim ACROSS a body of water at its surface (#191). If the
                // neighbor column is swimmable water whose surface sits roughly level with our current
                // height (a ground-level pool, or the next cell of one we're already swimming), connect
                // at that surface — so A* crosses the TOP of a pool instead of diving to the bottom and
                // back (which fights the controller's buoyancy toward the surface). This makes a
                // surface pool (e.g. the Halas central pool on the way to the Everfrost line) a
                // crossable swim rather than a drop to the pool floor the fall-guard refuses.
                if let Some(water) = &self.water {
                    // Probe downward for the first swimmable water within a step of the current
                    // floor — a pool's surface often sits a little BELOW the shore you wade in from
                    // (Halas's central pool surface is ~5u under the ice), so a 1u probe would miss
                    // it. Take that water's surface as the swim height.
                    let mut surf = None;
                    let mut z = cz - 1.0;
                    while z >= cz - STEP_H {
                        if water.is_water(b[0], b[1], z) { surf = water.surface_z(b[0], b[1], z); break; }
                        z -= 4.0;
                    }
                    if let Some(surf) = surf {
                        let nkey = (nc, nr, qf(surf));
                        // No chest-clearance requirement (like WATER DESCENT): you swim across open
                        // water at the surface, and a dry-walk clearance ray from the shore floor
                        // would snag on the ice/rock lip at the pool's edge — which is exactly what
                        // pushed A* to dive to the bottom instead. Cheaper than the descent, so A*
                        // now prefers crossing at the top; the controller collide-and-slides off any
                        // wall that happens to sit in the water.
                        if (surf - cz).abs() <= STEP_H && !closed.contains(&nkey) {
                            let step = (((dc * dc + dr * dr) as f32).sqrt()) * cell + (surf - cz).abs() * 0.5;
                            let tentative = g_cur + step;
                            if tentative < *g_score.get(&nkey).unwrap_or(&f32::MAX) {
                                g_score.insert(nkey, tentative);
                                came.insert(nkey, ckey);
                                floor_of.insert(nkey, surf);
                                heap.push(Node { f: tentative + h(nc, nr), c: nc, r: nr, fz: surf });
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
        let (goal_key, reached_goal) = match goal_key.or(goal_fallback) {
            Some(k) => (k, true),
            None => {
                // Partial-path fallback (#188): the goal cell is unreachable, but if the search got
                // meaningfully closer (≥1 cell of straight-line progress), walk to the nearest cell
                // it reached instead of returning "no route". The walker re-paths from there.
                let progressed = best_toward.is_some() && best_toward_h + 1.0 < h(sc, sr);
                match best_toward {
                    Some(bk) if allow_partial && progressed => {
                        tracing::info!("find_path: partial route toward goal (expanded={}, {:.0}->{:.0} cells from goal)",
                            expanded, h(sc, sr), best_toward_h);
                        (bk, false)
                    }
                    _ => {
                        tracing::info!("find_path: no route (expanded={}, cap={}, start_floor={}, goal_floor={})",
                            expanded, MAX_NODES, start_floor, goal_floor);
                        return None;
                    }
                }
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
        // Snap the final waypoint to the exact goal only when we actually reached the goal cell; a
        // partial path must end at the reachable cell, not clip toward an unreachable goal.
        if reached_goal {
            if let Some(last) = path.last_mut() { *last = [goal[0], goal[1], goal[2]]; }
        }
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

    /// eqoxide#213: compact_primitive must emit only the vertices a primitive references (from a
    /// shared pool) while preserving the exact triangles — this is what stops the qeynos 242×
    /// vertex-pool blowup that voided the zone.
    #[test]
    fn compact_primitive_drops_unreferenced_pool_vertices() {
        // A shared 5-vertex pool; the primitive's triangle uses only verts 4, 2, 0.
        let pool = vec![[0.0,0.0,0.0],[1.0,1.0,1.0],[2.0,2.0,2.0],[3.0,3.0,3.0],[4.0,4.0,4.0]];
        let normals = vec![[0.0,0.0,1.0]; 5];
        let uvs = vec![[0.0,0.0],[0.1,0.1],[0.2,0.2],[0.3,0.3],[0.4,0.4]];
        let indices = vec![4u32, 2, 0];

        let (p, n, u, idx) = compact_primitive(pool.clone(), normals, uvs.clone(), indices);
        assert_eq!(p.len(), 3, "only the 3 referenced verts survive (not all 5)");
        assert_eq!(n.len(), 3);
        assert_eq!(u.len(), 3);
        // Reconstruct the triangle through the remapped indices — must equal the originals.
        let tri: Vec<[f32;3]> = idx.iter().map(|&i| p[i as usize]).collect();
        assert_eq!(tri, vec![pool[4], pool[2], pool[0]], "triangle geometry preserved");
        let tri_uv: Vec<[f32;2]> = idx.iter().map(|&i| u[i as usize]).collect();
        assert_eq!(tri_uv, vec![uvs[4], uvs[2], uvs[0]], "per-vertex uvs follow the remap");

        // A shared index reuses one compacted vertex, not a duplicate.
        let (p2, _, _, idx2) = compact_primitive(pool.clone(), vec![[0.0,0.0,1.0];5], vec![[0.0,0.0];5], vec![2,2,4]);
        assert_eq!(p2.len(), 2, "the two distinct verts (2,4) → 2 outputs");
        assert_eq!(idx2, vec![0, 0, 1]);
    }

    /// A flooded pit must be exitable by SWIMMING UP: pit floor at z=0, a cliff wall
    /// up to the bank at z=10, water filling the pit to z=9. Without water the chest
    /// ray for the climb crosses the cliff face and the pit is sealed (qeynos2 moat,
    /// asset-server#14 / eqoxide#2 Case B). With water, A* must swim to the surface
    /// and haul out onto the bank.
    #[test]
    fn find_path_swims_up_out_of_a_flooded_pit() {
        let mesh = |positions: Vec<[f32; 3]>| MeshData {
            positions, normals: vec![], uvs: vec![],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // EQ WLD pos = [north, height, east].
        let pit_floor = mesh(vec![[0.0, 0.0, 0.0], [0.0, 0.0, 24.0], [24.0, 0.0, 24.0], [24.0, 0.0, 0.0]]);
        let cliff     = mesh(vec![[0.0, 0.0, 24.0], [24.0, 0.0, 24.0], [24.0, 10.0, 24.0], [0.0, 10.0, 24.0]]);
        let bank      = mesh(vec![[0.0, 10.0, 24.0], [0.0, 10.0, 48.0], [24.0, 10.0, 48.0], [24.0, 10.0, 24.0]]);
        let assets = ZoneAssets { terrain: vec![pit_floor, cliff, bank], objects: vec![], textures: vec![] };

        let start = [8.0, 12.0, 0.0];   // pit floor
        let goal  = [40.0, 12.0, 10.0]; // bank

        // Dry pit: sealed — the climb's chest ray crosses the cliff face.
        let dry = Collision::build(&assets, 4.0);
        assert!(dry.find_path(start, goal, 1.0, &[], false).is_none(),
            "dry pit should be sealed (no walkable exit)");

        // Flooded to z=9: swim up and haul out onto the bank.
        let mut wet = Collision::build(&assets, 4.0);
        wet.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(9.0))));
        let path = wet.find_path(start, goal, 1.0, &[], false);
        assert!(path.is_some(), "flooded pit must be exitable by swimming up to the bank");
        let last = *path.unwrap().last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0,
            "path should end at the bank goal, got {last:?}");
    }

    /// eqoxide#212: A* must refuse a ramp too steep to walk (it slides/wedges), while taking a
    /// gentle ramp of the same rise. Same geometry, only the ramp's run (steepness) differs.
    #[test]
    fn find_path_rejects_too_steep_ramp() {
        // MeshData pos = [north, up, east]; Collision maps to world [east, north, up].
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // Build: low floor (east -40..0, z=0), a ramp (east 0..RUN, z 0->30), high plateau
        // (east RUN..RUN+40, z=30). Start on the low floor, goal on the plateau. North spans 0..40.
        let scene = |run: f32| {
            let low  = quad(vec![[0.0, 0.0, -40.0], [40.0, 0.0, -40.0], [40.0, 0.0, 0.0], [0.0, 0.0, 0.0]]);
            let ramp = quad(vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [40.0, 30.0, run], [0.0, 30.0, run]]);
            let high = quad(vec![[0.0, 30.0, run], [40.0, 30.0, run], [40.0, 30.0, run + 40.0], [0.0, 30.0, run + 40.0]]);
            ZoneAssets { terrain: vec![low, ramp, high], objects: vec![], textures: vec![] }
        };
        let start = [-20.0, 20.0, 0.0]; // low floor (world [east,north,up])

        // Gentle ramp: 30u rise over 48u run = grade 0.625 < 1.2 → walkable.
        let gentle = Collision::build(&scene(48.0), 4.0);
        let goal_g = [48.0 + 20.0, 20.0, 30.0];
        let p_gentle = gentle.find_path(start, goal_g, 1.0, &[], false);
        assert!(p_gentle.is_some(), "a gentle (0.625) ramp must be walkable");
        let last = *p_gentle.unwrap().last().unwrap();
        assert!(last[2] > 20.0, "gentle path should reach the high plateau (z~30), got {last:?}");

        // Steep ramp: 30u rise over 16u run = grade 1.875 > 1.2 → A* must refuse to climb it, so
        // the plateau is unreachable (no partial route reaches the top tier).
        let steep = Collision::build(&scene(16.0), 4.0);
        let goal_s = [16.0 + 20.0, 20.0, 30.0];
        let full = steep.find_path(start, goal_s, 1.0, &[], false);
        assert!(full.is_none(), "a 1.875-grade ramp is too steep — A* must not route up it");
    }

    /// eqoxide#190: A* must route across a horizontal floor GAP a running jump can clear (a
    /// jump-edge), and must NOT invent a route across a gap wider than the jump reach.
    #[test]
    fn find_path_jumps_a_horizontal_gap() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // pos = [north, up, east]. Two z=0 platforms separated by a gap along east; north -40..40.
        let platform = |e0: f32, e1: f32| quad(vec![
            [-40.0, 0.0, e0], [40.0, 0.0, e0], [40.0, 0.0, e1], [-40.0, 0.0, e1]]);

        // find_path uses an 8u nav cell; reach ≈ 22.7u lands ≤ 2 cells (16u) out. Jumpable: an 8u
        // gap (east 8..16) — the far platform's cell sits 16u from the near edge. Only connection.
        let ok = ZoneAssets { terrain: vec![platform(-48.0, 8.0), platform(16.0, 64.0)], objects: vec![], textures: vec![] };
        let col = Collision::build(&ok, 4.0);
        let start = [-20.0, 0.0, 0.0]; // world [east, north, up], on platform A
        let goal  = [40.0, 0.0, 0.0];  // on platform B
        let path = col.find_path(start, goal, 1.0, &[], false)
            .expect("an 8u gap within jump reach must be routable via a jump-edge");
        let last = *path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0, "path reaches platform B, got {last:?}");
        // The route must contain a jump segment: a hop bigger than any adjacent-cell step
        // (≤ 8·√2 ≈ 11.3u at the 8u nav cell) — the gap crossing is ~16u.
        let has_jump = path.windows(2).any(|w| {
            ((w[1][0] - w[0][0]).powi(2) + (w[1][1] - w[0][1]).powi(2)).sqrt() > 12.0
        });
        assert!(has_jump, "route should include a jump segment across the gap: {path:?}");

        // Too wide: a 32u gap exceeds the jump reach → no route (must not fabricate one).
        let wide = Collision::build(
            &ZoneAssets { terrain: vec![platform(-48.0, 8.0), platform(40.0, 88.0)], objects: vec![], textures: vec![] },
            4.0);
        assert!(wide.find_path([-20.0, 0.0, 0.0], [60.0, 0.0, 0.0], 1.0, &[], false).is_none(),
            "a 32u gap exceeds jump reach — A* must not route across it");
    }

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
        let path = col.find_path([5.0, 5.0, 0.0], [15.0, 5.0, 0.0], 1.0, &[], false)
            .expect("a route around the wall should exist");
        let last = *path.last().unwrap();
        assert!((last[0] - 15.0).abs() < 1.5 && (last[1] - 5.0).abs() < 1.5, "ends at goal: {last:?}");
        assert!(path.iter().any(|p| p[1] > 12.0), "path must detour north through the gap: {path:?}");
    }

    #[test]
    fn find_path_returns_partial_route_when_goal_is_walled_off() {
        // 200x200 floor at z=0 (big enough for the 8u nav grid to make real progress).
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // FULL wall at east=100 spanning the whole north extent (0..200) — no gap, so the goal is
        // sealed off with no route to it.
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 100.0], [200.0, 0.0, 100.0], [200.0, 20.0, 100.0], [0.0, 20.0, 100.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0]; // sealed behind the wall at east=100
        // No full route exists.
        assert!(col.find_path(start, goal, 1.0, &[], false).is_none(), "goal is walled off — no full route");
        // But a partial route toward the goal does (#188): it advances east toward the wall and
        // stops on the near side (never crossing east=100) instead of returning "no route".
        let partial = col.find_path(start, goal, 1.0, &[], true).expect("partial route toward the goal");
        let last = *partial.last().unwrap();
        assert!(last[0] > start[0] + 30.0, "partial route makes real progress toward the goal: {last:?}");
        assert!(last[0] < 100.0, "partial route stops on the near side of the wall: {last:?}");
    }

    #[test]
    fn find_path_swims_across_a_surface_pool_instead_of_diving() {
        // Positions are [north, up, east]. A deep pool bottom under the whole span, with dry shores
        // laid on top at z=0 at each end, and a surface-level water body between them.
        let quad = |n0: f32, n1: f32, e0: f32, e1: f32, up: f32| MeshData {
            positions: vec![[n0, up, e0], [n1, up, e0], [n1, up, e1], [n0, up, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let pool_bottom = quad(0.0, 40.0, 0.0, 160.0, -92.0); // deep floor, east 0..160
        let near_shore  = quad(0.0, 40.0, 0.0, 40.0, 0.0);    // dry, east 0..40
        let far_shore   = quad(0.0, 40.0, 120.0, 160.0, 0.0); // dry, east 120..160
        let mut col = Collision::build(
            &ZoneAssets { terrain: vec![pool_bottom, near_shore, far_shore], objects: vec![], textures: vec![] }, 8.0);
        // SUNKEN pool: water surface at z=-8, a few units BELOW the z=0 shores you wade in from
        // (like Halas's central pool under the ice) — so the swim edge has to probe down to find it.
        col.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(-8.0))));

        let start = [20.0, 20.0, 0.0];   // near shore
        let goal  = [140.0, 20.0, 0.0];  // far shore, across the pool
        let path = col.find_path(start, goal, crate::movement::PLAYER_RADIUS, &[], false)
            .expect("a swim route across the surface pool should exist");
        // It reaches the far shore...
        let last = *path.last().unwrap();
        assert!((last[0] - 140.0).abs() < 12.0, "ends at the far shore: {last:?}");
        // ...at the SURFACE (~ -8), never diving toward the -92 pool bottom.
        let deepest = path.iter().map(|w| w[2]).fold(f32::MAX, f32::min);
        assert!(deepest > -20.0, "route stays near the surface, not the pool bottom: deepest={deepest} path={path:?}");
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

        let direct = col.find_path(start, goal, 1.0, &[], false).expect("open route exists");
        let skirt  = col.find_path(start, goal, 1.0, &npc, false).expect("aggro route still exists (mild penalty)");

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
        let mut col = Collision::build(&za, 32.0);
        // Attach the zone's water map like production does (app.rs) — the earlier run of
        // this diagnostic skipped it and mis-reported the moat as having no water volume.
        let wtr_dir = std::path::Path::new(&p).parent().unwrap().join("maps/water");
        col.set_water(crate::region_map::RegionMap::load(&wtr_dir, "qeynos2").map(std::sync::Arc::new));
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
            match col.find_path(start, goal, crate::movement::PLAYER_RADIUS, &[], false) {
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
        // Case B — the char is in the qeynos2 moat at z=-16. The earlier "sealed pit / no water
        // volume" verdict was wrong on both counts: the zone DOES have water (delivered via
        // maps/water/qeynos2.wtr — this diagnostic just never attached it), and with the WATER
        // ASCENT nav edge the moat is exitable: swim up at the south end and haul out onto the
        // z=+1 bank (see B2), from which the city center is reachable (B5/B7). The original
        // street goal below still probes NONE because that west-gate strip is disconnected from
        // the city center even street→street (B6) — an unreachable GOAL, not a moat problem.
        probe("B moat → west-gate street (goal itself disconnected, see B6)", [-502.3, -141.3, -16.0], [-600.0, -141.0, -5.0]);
        let within = col.find_path([-502.3, -141.3, -16.0], [-490.0, -100.0, -16.0], crate::movement::PLAYER_RADIUS, &[], false);
        eprintln!("  moat floor traversable (start → 40u north @z=-16): {}",
            within.map(|p| format!("{} waypoints", p.len())).unwrap_or_else(|| "NONE".into()));
        for gx in [-560.0f32, -580.0, -600.0] {
            let r = col.find_path([-502.3, -141.3, -16.0], [gx, -141.0, -8.0], crate::movement::PLAYER_RADIUS, &[], false);
            eprintln!("  moat → street x={gx}: {}",
                r.map(|p| format!("{} waypoints", p.len())).unwrap_or_else(|| "NONE".into()));
        }
        eprintln!("  zone has a water volume: {}", col.water.is_some());

        // Moat exit scan: walk the whole moat water region and report every column where a
        // haul-out is geometrically possible (a neighbor floor within STEP_H of the water
        // surface and a clear chest ray from swim height). If this prints nothing, the
        // collision genuinely has no exit and the swim-up nav edge can't help.
        if let Some(w) = col.water.clone() {
            let mut found = 0;
            let mut y = -260.0f32;
            while y < -20.0 {
                let mut x = -520.0f32;
                while x < -420.0 {
                    if w.is_water(x, y, -11.0) {
                        let mut surface = -16.0f32;
                        while surface < 40.0 && w.is_water(x, y, surface + 2.0) { surface += 2.0; }
                        for (dx, dy) in [(-8.0f32,0.0),(8.0,0.0),(0.0,-8.0),(0.0,8.0),(-8.0,-8.0),(8.0,8.0),(-8.0,8.0),(8.0,-8.0)] {
                            let (bx, by) = (x + dx, y + dy);
                            for nf in col.column_floors(bx, by, surface, 20.0, 4.0) {
                                if nf > surface + 20.0 || nf <= -15.0 { continue; }
                                let ray_z = surface.max(nf - 20.0);
                                if col.path_clear([x, y, ray_z + 3.0], [bx, by, nf + 3.0], crate::movement::PLAYER_RADIUS) {
                                    eprintln!("  EXIT candidate: swim ({x:.0},{y:.0}) surface {surface:.1} -> floor {nf:.1} at ({bx:.0},{by:.0})");
                                    found += 1;
                                }
                            }
                        }
                    }
                    x += 8.0;
                }
                y += 8.0;
            }
            eprintln!("  moat exit candidates: {found}");
        }
        // Route segments: can A* reach the SE-bank exit, and does the bank connect onward?
        probe("B2 moat → SE bank (swim-up exit)", [-502.3, -141.3, -16.0], [-488.0, -244.0, 1.0]);
        probe("B3 SE bank → west street",         [-488.0, -244.0, 1.0],   [-600.0, -141.0, -8.0]);
        probe("B4 moat floor → SE moat water",    [-502.3, -141.3, -16.0], [-488.0, -236.0, -14.0]);
        probe("B5 SE bank → city center",         [-488.0, -244.0, 1.0],   [0.0, 0.0, 3.0]);
        probe("B6 west street → city center",     [-600.0, -141.0, -8.0],  [0.0, 0.0, 3.0]);
        probe("B7 moat → city center",            [-502.3, -141.3, -16.0], [0.0, 0.0, 3.0]);
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
