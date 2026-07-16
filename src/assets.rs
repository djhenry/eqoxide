//! Zone + texture asset loading.
//!
//! Loads EQ zone GLB/PNG assets into CPU-side `MeshData`/`TextureData`, instances
//! placed objects (buildings, etc.) from the zone's ActorInstance fragments, and indexes equipment
//! textures. See `docs/zone-rendering.md`.
//!
//! The collision grid built from this data (`Collision::build`) and its A* pathfinding model — the
//! `floor_z`/`nearest_floor` (grounding), `nearest_hit_t`/`segment_blocked` (camera + nameplate
//! occlusion), `path_clear` (movement gating), and `find_path` (A* routing) queries — live in
//! [`crate::nav::collision`] (cleanup step 4). See `docs/collision-system.md`.

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

/// Sentinel `MeshData.texture_name` marking the dedicated collision geometry baked into a
/// zone GLB as a mesh named `__collision__`. The asset-server/converter emits every SOLID
/// face (including INVIS — invisible-but-solid zone boundaries, invisible walls, doorframes)
/// here while EXCLUDING PASSABLE faces (water surfaces, foliage). `from_glb` tags the loaded
/// mesh with this name so the renderer skips drawing it and `Collision::build` consumes it
/// for collision instead of the rendered terrain.
pub const COLLISION_MESH_TAG: &str = "__collision__";

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
