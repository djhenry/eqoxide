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

/// The DETERMINISTIC runaway bound for a whole plan: a maximum number of node expansions (#394).
///
/// # This REPLACES a wall-clock budget, and that is the whole point
///
/// The coarse worker used to carry `WORKER_PLAN_BUDGET_MS = 5_000` — a five-second wall clock. A wall
/// clock makes the planner's answer **a function of how fast the machine is**: on a loaded runner the
/// 5 s expired before a big zone's frontier closed, so a genuinely-unreachable goal came back
/// `Exhausted(Deadline)` ("I don't know") on a slow box and `Unreachable(SearchClosed)` ("no route") on
/// a fast one. That is not a lie — `Exhausted` is honest — but it is **nondeterministic**, and it is
/// why `main`'s CI was intermittently red (`an_unreachable_goal_reports_unreachable_not_a_partial_route`
/// failing under load). #377's claim that the budget was "deleted" and the planner "deterministic" was
/// simply false: the budget was raised 150 ms → 5 s and moved off the net thread, not removed.
///
/// A NODE cap has the identical runaway protection — it stops a pathological search from pinning a
/// core — but it is **machine-independent**: the same query expands the same nodes and returns the
/// same `PlanOutcome` on my box, on CI, and on the user's, whatever the load. Hitting it yields
/// `Exhausted(NodeCap)`, an honest and *reproducible* "I stopped looking".
///
/// **Chosen by measurement** (`worst_case_reachable_component`, over the biggest baked zones in the
/// test corpus): the largest **measured-corpus** reachable-component close is **everfrost, 1,121,438
/// nodes** (its 8 u nav grid is ~1.1M cells). That is the worst legitimate whole-zone "no route" the
/// cap must let through as `SearchClosed` — and note it already EXCEEDED the previous
/// `MAX_NODES = 1_000_000`, so main was silently truncating everfrost's honest closes into false
/// `Exhausted`. This cap is ~7× above it, so a legitimate whole-zone close reaches `SearchClosed` with
/// headroom, while still bounding a true runaway.
///
/// **Caveat, stated honestly:** everfrost is the biggest zone *in the corpus*, NOT the biggest in RoF2
/// — larger outdoor zones exist and are unmeasured. But the residual risk is small and bounded: (1) a
/// REACHABLE goal is found by goal-directed A* long before the cap (the admissible heuristic pulls the
/// search toward the goal, so it does not explore the whole component), so a bigger zone does not make
/// a reachable goal false-`Exhausted`; (2) the only failure is an UNREACHABLE goal in a >8M-node
/// component reporting `Exhausted(NodeCap)` ("I don't know") instead of `Unreachable(SearchClosed)`
/// ("no") — which is still HONEST, just less precise. So 8M is a precision floor, not a safety floor.
///
/// It is the cap for the ENTIRE plan (`plan_path` makes up to 13 A* calls sharing one `PlanCtx`
/// budget), so the plan is bounded by one budget, not one-per-call (#340).
pub const MAX_NODES: usize = 8_000_000;

/// Deterministic node cap for the FINE local tier (#394).
///
/// The fine search is bounded SPATIALLY — a 40 u window at 2 u cells, ~1257 XY cells × a few z-tiers —
/// so its frontier genuinely closes at ~800–3700 nodes in practice (measured). This cap is therefore a
/// pure runaway backstop that a real fine plan never hits; it exists so a pathological zone cannot spin
/// the search unboundedly, and — like the coarse cap — it is a node count, not a clock, so the outcome
/// is the same on every machine.
///
/// **Why #382 moves this tier off the net thread even though it is already deterministic:** the fine
/// search's cost is dominated by PER-NODE collision work (`column_floors` + capsule sweeps), NOT by node
/// count. Measured worst case (release, corpus): a 1.34 s fine plan that closed just ~3681 nodes —
/// ~366 µs/node in dense stacked geometry. So there is **no node cap that bounds this search's WALL TIME
/// without cutting legitimate routes** (normal fine searches close ~800–1200 nodes). A cap keeps the
/// answer honest and deterministic; only moving OFF the net thread keeps an occasional 1.3 s fine plan
/// from stalling the network loop. Nothing waits on the fine worker, and the walker keeps steering on
/// the last good plan meanwhile (#382).
pub const NET_TIER_NODE_CAP: usize = 40_000;

/// Per-plan context for `find_path_res`: the things that must be shared across the several A* calls
/// one logical plan makes, rather than re-armed per call.
#[derive(Clone, Default)]
pub struct PlanCtx {
    /// The plan's runaway bound: a maximum number of node expansions **across the WHOLE plan** (#340,
    /// #394). `plan_path` makes up to 13 A* calls (1 primary + a 12-point `StartIsolated` re-anchor
    /// ring), and `search_tiered` makes up to 2 clearance passes inside each — this is the budget for
    /// ALL of them together, not one each.
    ///
    /// **This is a NODE COUNT, not a wall clock, and it deliberately CANNOT be a wall clock (#394).**
    /// A wall-clock deadline made the planner's answer depend on machine speed: a genuinely-unreachable
    /// goal in a big zone came back `Unreachable(SearchClosed)` on a fast box and `Exhausted(Deadline)`
    /// on a slow/loaded one — the same question, two answers, which is what made CI intermittently red.
    /// A node cap is reproducible: the same query expands the same nodes and returns the same
    /// `PlanOutcome` on every machine. There is no `Option<Instant>` field here, and there is no method
    /// that builds one, so a clock-dependent search is not merely discouraged — it is unrepresentable.
    ///
    /// `None` = the global `MAX_NODES` backstop. A caller may set a TIGHTER cap (the tiers do — see
    /// [`NET_TIER_NODE_CAP`]). Whichever bites, the outcome is `Exhausted(NodeCap)` — an honest
    /// "I stopped looking", never a "no route".
    pub node_cap: Option<usize>,
    /// The plan-wide RUNNING TOTAL of node expansions, shared by every A* call in the plan (#394 review).
    ///
    /// This is what makes `node_cap` a WHOLE-PLAN bound rather than a per-call one. On `main` the
    /// wall-clock version got plan-wide bounding for free: `deadline` was a single absolute `Instant`,
    /// so all 13 calls checked the *same* moment. A node count has no absolute reference — expansions
    /// accumulate — so the running total must be shared explicitly. Every `astar` increments this
    /// counter and stops the plan when it passes `node_cap`; a plan that fans out to 13 calls therefore
    /// still costs at most `node_cap` expansions total, not `node_cap × 13`. The first plan owner
    /// (`plan_path`, or a standalone `find_path_ex`/`find_path_res`) materialises it via
    /// [`PlanCtx::ensure_budget`]; every call it spawns clones the same `Arc` and so shares the count.
    ///
    /// `None` only in a `PlanCtx` that has not yet entered a plan (e.g. `PlanCtx::worker()` before it
    /// reaches `find_path_ex`); the plan owner fills it in before the first search runs.
    pub expanded: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
    /// Zone-point index of a `DRNTP` zone-line region we are routing to. When set, A* accepts
    /// arrival at ANY cell whose (XY, floor) lies inside that region — not just the one goal cell
    /// at the right tier. A region's representative point is an interior point of a VOLUME, so its
    /// z is structurally never a floor height and a single cell+tier test on it is unsound (#229).
    pub goal_region: Option<i32>,
}

impl PlanCtx {
    /// A context bounded by a fresh node cap, shared across the plan's A* calls.
    pub fn with_node_cap(cap: usize) -> Self {
        PlanCtx { node_cap: Some(cap), ..Default::default() }
    }
    /// Materialise the plan-wide expansion counter if it isn't already present, and return the ctx.
    ///
    /// Called by the PLAN OWNERS — `plan_path`, and standalone `find_path_ex`/`find_path_res` — so that
    /// every A* call in one logical plan shares ONE running total. Idempotent: a ctx that already has a
    /// counter (because `plan_path` set it before fanning out to `find_path_ex`) keeps that same shared
    /// `Arc`, which is exactly how the 13 calls come to share a budget.
    pub fn ensure_budget(mut self) -> Self {
        if self.expanded.is_none() {
            self.expanded = Some(std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)));
        }
        self
    }
    /// The pathfinding worker's context: bounded only by the global [`MAX_NODES`] backstop, which is
    /// generous enough that a real whole-zone close reaches `SearchClosed` (chosen by measurement). Its
    /// answer no longer depends on the clock (#394); nothing real-time waits on this thread.
    pub fn worker() -> Self { Self::default() }
    /// The fine local tier's context: a node-cap backstop (see [`NET_TIER_NODE_CAP`]); the tier is
    /// really bounded by its 40u spatial window.
    pub fn net_tier() -> Self { Self::with_node_cap(NET_TIER_NODE_CAP) }
    pub fn with_goal_region(mut self, idx: Option<i32>) -> Self {
        self.goal_region = idx;
        self
    }
}

/// Why a search stopped WITHOUT closing its frontier: it hit its node cap. Means "I don't know",
/// never "no".
///
/// **This used to have a second variant, `Deadline` (a wall-clock timeout), and it was deleted on
/// purpose (#394).** A wall-clock limit made the planner's answer machine-speed-dependent — the same
/// unreachable goal reported `Unreachable` on a fast box and `Exhausted(Deadline)` on a slow one. There
/// is now only ONE way a search can be cut short, it is a deterministic node count, and a wall clock
/// cannot be reintroduced because `PlanCtx` can no longer hold one. Keeping the enum (rather than
/// folding it away) leaves `PlanOutcome::Exhausted` a clean place to name future *deterministic* limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanLimit {
    /// The node cap (`PlanCtx::node_cap`, or the global `MAX_NODES`) was hit.
    NodeCap,
}

impl PlanLimit {
    pub fn as_str(self) -> &'static str {
        match self {
            PlanLimit::NodeCap => "search_node_cap",
        }
    }
}

/// Why no route exists. Every variant is a DEFINITIVE, falsifiable "no" — the search either never
/// had a valid question to answer, or it closed its whole reachable frontier without finding the
/// goal. A timeout is NEVER one of these (that's [`PlanLimit`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoRoute {
    /// No collision geometry loaded (still zoning) — nothing can be planned at all.
    NoGeometry,
    /// The GOAL has no walkable floor under or near it: it is inside solid rock, off the mesh, or
    /// floating in the air far above any ground. No amount of searching can accept arrival there,
    /// so we fail immediately and loudly instead of flooding the grid and returning a greedy
    /// partial that the walker drives into a wall (#337).
    GoalNotWalkable,
    /// The START's reachable component is a handful of cells — the character is boxed in (standing
    /// inside a tree trunk / on a slope face). The caller (`plan_path`) retries from a re-anchored
    /// start before believing this.
    StartIsolated,
    /// The search CLOSED its entire reachable frontier and the goal was not in it. This is the real
    /// "you cannot walk there from here".
    SearchClosed,
}

impl NoRoute {
    pub fn as_str(self) -> &'static str {
        match self {
            NoRoute::NoGeometry      => "no_geometry",
            NoRoute::GoalNotWalkable => "goal_not_walkable",
            NoRoute::StartIsolated   => "start_isolated",
            NoRoute::SearchClosed    => "search_closed",
        }
    }
}

/// The HONEST outcome of a path plan (#337, #356).
///
/// The old planner returned `Option<Vec<Waypoint>>`, which conflated three completely different
/// answers into one `None`/partial: "here is your route", "there is no route", and "I gave up".
/// The walker could not tell them apart, so it silently walked a timed-out partial route into a
/// wall, retried 8×, and froze at `nav_state: blocked` — a lie that disguised the real nav root
/// cause for months. These three variants are the whole point of the change.
#[derive(Debug, Clone, PartialEq)]
pub enum PlanOutcome {
    /// A COMPLETE route that reaches the goal. The only variant the walker may treat as a plan.
    Route(Vec<[f32; 3]>),
    /// DEFINITIVE: no route exists. An honest, falsifiable "no" the agent can act on.
    Unreachable(NoRoute),
    /// The search was cut short (`limit`) before closing its frontier: "I DON'T KNOW", not "no".
    /// `progress` is a partial route toward the reachable frontier, present ONLY when it makes
    /// GENUINE goal-ward progress (see `PARTIAL_MIN_UNITS`) — walk it and re-plan from the far end.
    Exhausted { limit: PlanLimit, progress: Option<Vec<[f32; 3]>> },
}

impl PlanOutcome {
    /// The COMPLETE route, if this outcome is one. A partial route is deliberately NOT returned
    /// here: treating it as a plan is exactly the #337 lie.
    pub fn route(&self) -> Option<&Vec<[f32; 3]>> {
        match self { PlanOutcome::Route(p) => Some(p), _ => None }
    }
    /// A machine-readable reason, surfaced to agents via `nav_reason` on GET /v1/observe/debug.
    pub fn reason(&self) -> &'static str {
        match self {
            PlanOutcome::Route(_) => "route",
            PlanOutcome::Unreachable(r) => r.as_str(),
            PlanOutcome::Exhausted { limit, .. } => limit.as_str(),
        }
    }
}

/// The HONEST outcome of the FINE LOCAL steering search — the bounded 2 u tier that actually steers
/// the character along the last ~40 u of the committed coarse route (#382).
///
/// # Why this is NOT `PlanOutcome`
///
/// Two differences, and both of them are safety properties rather than taste:
///
/// 1. **A bounded search's "no" is a statement about its WINDOW, never about the goal.** The fine
///    search only ever closes the frontier *inside* `LOCAL_BOUND` (40 u). "I could not reach the
///    carrot" therefore means "not through this 40 u window" — it is *not* evidence that the goal is
///    unreachable, and it must never be able to become `nav_state: no_path`. Giving this tier a
///    `PlanOutcome` would put an `Unreachable` variant in the hands of the steering loop, and
///    `Unreachable` is the one word in this codebase that means a **definitive, falsifiable no**.
///    There is deliberately no way to spell that here.
/// 2. **Every variant carries `steer`.** `PlanOutcome::Unreachable` carries no waypoints on purpose
///    (walking a partial you have proven leads nowhere is the #337 lie). But the fine tier's partial
///    is not a route proposal — it is a *steering hint*, re-planned continuously, and it is load-
///    bearing: with it wiped, a halas swimmer floating at the water's edge stopped swimming and
///    wedged at the shoreline while the coarse planner cheerfully re-issued a perfect 78-waypoint
///    route across the water, every tick, for 8 attempts (#377 review, N1). "I cannot reach the
///    carrot" does not imply "I cannot usefully move."
///
/// # The distinction that matters
///
/// [`LocalOutcome::NoWayThrough`] (the window's frontier CLOSED) and [`LocalOutcome::Exhausted`] (the
/// search was CUT SHORT) look identical from outside — both are "the steer path stops short of the
/// carrot" — and for as long as the fine tier ran under a 150 ms wall clock they *were* identical:
/// one `Option<Vec<_>>`, no way to ask which. The walker armed the proactive coarse re-plan (#246) on
/// both, so **a timeout was silently laundered into "the coarse route ahead is blocked"**. Under CPU
/// load that fired on routes that were perfectly threadable. Telling the two apart is the whole point
/// of this type — see `navigation::arms_coarse_replan`.
#[derive(Debug, Clone, PartialEq)]
pub enum LocalOutcome {
    /// A complete fine route from the character to the carrot. The healthy case.
    Threaded(Vec<[f32; 3]>),
    /// The window's frontier CLOSED without reaching the carrot: inside this 40 u window there is
    /// genuinely no way through to it (the coarse corridor skims something the 8 u grid missed).
    /// A falsifiable *local* no — and the ONLY outcome that may arm a proactive coarse re-plan.
    NoWayThrough {
        steer: Vec<[f32; 3]>,
        /// Which flavour of local dead-end (`search_closed`, `start_isolated`, `goal_not_walkable`,
        /// `no_geometry`). Reported verbatim so an agent can tell "the corridor is walled" from
        /// "*I* am the one who is wedged".
        why:   NoRoute,
    },
    /// The search was CUT SHORT by `MAX_NODES` before closing its window: "**I don't know**", not
    /// "no". It must never arm a coarse re-plan, and it must never reach the agent as `no_path`.
    ///
    /// There is no `PlanLimit::Deadline` here in practice — the fine tier arms no wall clock
    /// (`PlanCtx::default()`), which is exactly what #382 deleted — but the variant is typed on
    /// `PlanLimit` so that a limit, whatever its kind, can only ever be spelled as "I stopped
    /// looking".
    Exhausted { limit: PlanLimit, steer: Vec<[f32; 3]> },
}

impl LocalOutcome {
    /// The waypoints to STEER along this tick — a complete fine route, or the best partial toward the
    /// carrot. Always available (possibly empty); the walker never has to wait for it.
    pub fn steer(&self) -> &[[f32; 3]] {
        match self {
            LocalOutcome::Threaded(p) => p,
            LocalOutcome::NoWayThrough { steer, .. } | LocalOutcome::Exhausted { steer, .. } => steer,
        }
    }
    /// Did the fine plan actually REACH its carrot?
    pub fn threaded(&self) -> bool { matches!(self, LocalOutcome::Threaded(_)) }
    /// The state word published as `nav_local.state` on GET /v1/observe/debug.
    ///
    /// **None of these is `no_path`, and none of them can become it.** A bounded window cannot prove
    /// a goal unreachable, so this tier is structurally incapable of saying so — see the type docs.
    pub fn state(&self) -> &'static str {
        match self {
            LocalOutcome::Threaded(_)       => "threaded",
            LocalOutcome::NoWayThrough { .. } => "no_way_through",
            LocalOutcome::Exhausted { .. }  => "exhausted",
        }
    }
    /// The machine-readable WHY, surfaced as `nav_local.reason`.
    pub fn reason(&self) -> &'static str {
        match self {
            LocalOutcome::Threaded(_)          => "threaded",
            LocalOutcome::NoWayThrough { why, .. } => why.as_str(),
            LocalOutcome::Exhausted { limit, .. }  => limit.as_str(),
        }
    }
}

/// The raw result of ONE A* run, before it is turned into an honest [`PlanOutcome`].
#[derive(Debug, Default)]
struct Search {
    /// `(route, reached_goal)`. `reached_goal == false` = a PARTIAL route toward the frontier.
    path:     Option<(Vec<[f32; 3]>, bool)>,
    /// `Some` = the search was CUT SHORT and its frontier is NOT closed, so "the goal was not
    /// reached" means *I don't know*. `None` = the frontier closed (or the question was invalid) —
    /// only then may a missing route be reported as "no route exists".
    limit:    Option<PlanLimit>,
    /// Set when we can name WHY there is no route (invalid goal / boxed-in start). `None` with
    /// `limit: None` and no path = the frontier simply closed without the goal in it.
    no_route: Option<NoRoute>,
    /// Straight-line ground (units) toward the goal that a partial route actually closes.
    progress: f32,
    /// Nodes whose expansion completed — how big the explored component is.
    closed_n: usize,
}

impl Search {
    fn no_route(r: NoRoute) -> Self { Search { no_route: Some(r), ..Default::default() } }
}

/// The minimum straight-line ground (units) a PARTIAL route must close toward the goal before the
/// walker is allowed to walk it. The old bar was ONE nav cell (8u) — so a search that inched a
/// single cell toward an unreachable goal produced a "route" the walker drove into a wall and then
/// wedged on (#337). A partial exists to let a long journey be walked in stages, not to let a
/// wedged character shuffle; 48u = 6 nav cells is a stage, 8u is a shuffle.
pub const PARTIAL_MIN_UNITS: f32 = 48.0;

pub struct Collision {
    tris:      Vec<[[f32; 3]; 3]>,
    /// Per-triangle face-normal Z (normalized), parallel to `tris`. Sign = facing: `> 0` is an
    /// UP-facing surface (a floor you can stand on), `< 0` a DOWN-facing one (a ceiling / the
    /// underside of a bridge). Nav used to treat any surface a vertical ray crossed as a floor —
    /// which is how A* ended up standing on qcat's ceiling and planning routes through solid rock
    /// (#329). Computed once at build; the ray tests filter on it. (~4 bytes/tri.)
    tri_nz:    Vec<f32>,
    cells:     Vec<Vec<u32>>,
    origin:    [f32; 2], // (east, north) of cell (0,0) corner
    cell_size: f32,
    cols:      usize,
    rows:      usize,
    /// Z extent of the whole mesh. The floor-normal filter's safety valve (`column_hits`) has to ask
    /// "is there anything BENEATH this surface?" of the FULL COLUMN, not of the caller's query
    /// window — a window is only ~100u tall and a cavern roof's floor is often further down than
    /// that. Bounds let a column probe span the zone regardless of what the caller asked for.
    z_min:     f32,
    z_max:     f32,
    /// How many times the empty-column fallback in `column_hits` has fired since zone load. The
    /// fallback is a DEGRADED path — it answers from inverted (mis-wound) art — so an agent must be
    /// able to see that it is running: this is what `/v1/observe/debug` reports as `nav_degraded`.
    /// Relaxed: a diagnostic counter, never read for control flow.
    fallback_hits: std::sync::atomic::AtomicU64,
    /// How many routes only existed at the MINIMUM clearance (`PLAYER_RADIUS`) — i.e. threaded a
    /// narrow door or a tight bridge with no margin to spare. Surfaced as `nav_tight` so an agent is
    /// never silently handed a riskier path than it thinks (`search_tiered`).
    tight_plans: std::sync::atomic::AtomicU64,
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

/// The clearance a route is planned at BY DEFAULT — deliberately larger than the character.
///
/// Fitting is not walking. A route planned at exactly `PLAYER_RADIUS` is allowed to skim a wall, a
/// cliff lip or the edge of a bridge with *zero* margin, and the walker — which slides on contact
/// and gets shoved around by server position corrections — falls off it. So plan with room, and
/// fall back to the minimum only where the roomy route does not exist (`search_tiered`).
///
/// **2 × `PLAYER_RADIUS`**: one radius to fit, one radius of margin. Chosen by measuring the
/// fallback rate over 1200 start/goal pairs in the cached zones — the fraction of routes that only
/// exist at the minimum clearance, i.e. where the second A* pass is spent for nothing:
///
/// | preferred | qeynos2 | gfaydark | freportw | akanon |
/// |-----------|---------|----------|----------|--------|
/// | 1.5 ×     |   0 %   |    0 %   |    8 %   |  32 %  |
/// | **2.0 ×** | **0 %** |  **3 %** | **16 %** |**33 %**|
/// | 2.5 ×     |   0 %   |     —    |   18 %   |  39 %  |
/// | 3.0 ×     |   0 %   |     —    |   28 %   |  48 %  |
///
/// Routability is identical at every value (the fallback guarantees it) — what moves is how often
/// the roomy tier fails. At 2× the generous tier still carries the large majority of routes in the
/// open and city zones; by 3× the fallback is close to a coin-flip in the tight indoor ones
/// (Ak'Anon's gnome tunnels), which is two searches to answer what one could. 2× buys a full
/// body-width of standing room without making the exception the rule.
pub const NAV_PREFERRED_CLEARANCE: f32 = crate::movement::PLAYER_RADIUS * 2.0;

/// **D-2 (`is_standable`, #375): the two knobs of the shared floor predicate.** A surface is standable
/// ground, FACING-BLIND, iff `|nz| >= NAV_NEAR_HORIZONTAL` (flat enough to stand on) AND it has
/// `NAV_AGENT_HEIGHT` of open space above it before the next SOLID surface (else it is under a ceiling,
/// not standing room). This replaces the winding-sign filter (`nz <= 0` deleted real inverted-art
/// floor — the qcat live wedge, #375) AND its `column_bottom` recovery valve.
///
/// `NAV_NEAR_HORIZONTAL` is tied to the walk-grade limit: a unit normal's `|z|` for a surface at grade
/// `g` is `1/sqrt(1+g²)`, and `MAX_WALK_GRADE = 1.2` (the astar climb cap) gives `1/sqrt(1+1.44) ≈
/// 0.64`. So a surface `is_standable` rejects for flatness is exactly one astar's grade limit would
/// reject anyway — no new seal there.
///
/// `NAV_AGENT_HEIGHT` is the clearance a standing character needs. It must EXCEED a real ceiling's
/// slab-gap (a room ceiling has its roof right above → tiny headroom → rejected) yet stay BELOW a real
/// room's height (or a low room's floor would be wrongly rejected → seal). The controller's own chest
/// collision ray sits at `foot + 4.0` (`movement.rs`), so ~5u is the clearance a body actually needs;
/// this is measured against route-success (≥ 99.50%) before shipping.
///
/// Both belong on the shared `Body` (PR-A). Defined here until PR-A lands — do NOT invent a second copy.
pub const NAV_NEAR_HORIZONTAL: f32 = 0.64;
pub const NAV_AGENT_HEIGHT: f32 = 5.0;

/// The share of a plan's node budget the GENEROUS clearance pass may spend before it is abandoned in
/// favour of the minimum-clearance pass that actually decides the answer.
///
/// The roomy tier is an OPTIMISATION — a nicer route when one is cheaply available. The minimum tier
/// is the one that knows whether a route exists at all, so it must never be starved by the tier that
/// merely prefers a better one. Both passes share the CALLER'S single node budget (see `search_tiered`):
/// giving each its own would make one plan cost two budgets.
const GENEROUS_BUDGET_SHARE: f32 = 0.4;

/// The generous pass's node cap: a SLICE of the caller's budget, **never a fresh one** (#394).
///
/// One plan, one budget. A pass that arms its own cap makes a plan cost N budgets instead of one. The
/// cap is subdivided ONCE by the caller of this function so the two passes together stay within the one
/// budget the caller set.
///
/// `None` in → `None` out: an unbudgeted plan stays unbudgeted (bounded only by the global `MAX_NODES`
/// backstop); this function must never INVENT a cap, only subdivide one. A node cap, unlike the
/// wall-clock deadline this replaced, does not "run down" between the passes — it is a fixed budget,
/// so the split is a plain fraction with no clock to drift.
fn generous_node_cap(caller: Option<usize>) -> Option<usize> {
    caller.map(|cap| ((cap as f32) * GENEROUS_BUDGET_SHARE) as usize)
}

/// Plan cell size at or below which A* validates an edge by sweeping the character's whole
/// collision volume instead of casting a centre ray — see `Collision::edge_clear` for the measured
/// reason this is not simply "always". Sits above `navigation::LOCAL_CELL` (2u, the fine tier) and
/// below the coarse whole-zone grid (8u).
pub(crate) const SWEPT_EDGE_MAX_CELL: f32 = 4.0;

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

        // XY bounds (for the broad-phase grid) and Z bounds (so a column probe can span the whole
        // mesh — see `z_min`/`z_max`).
        let mut min = [f32::MAX; 2];
        let mut max = [f32::MIN; 2];
        let (mut z_min, mut z_max) = (f32::MAX, f32::MIN);
        for t in &tris {
            for v in t {
                if v[0] < min[0] { min[0] = v[0]; }
                if v[1] < min[1] { min[1] = v[1]; }
                if v[0] > max[0] { max[0] = v[0]; }
                if v[1] > max[1] { max[1] = v[1]; }
                if v[2] < z_min { z_min = v[2]; }
                if v[2] > z_max { z_max = v[2]; }
            }
        }
        // Face-normal Z per triangle (see `tri_nz`). The WLD→world map (x,y,z) → (z,x,y) is a cyclic
        // permutation (determinant +1), so it PRESERVES winding — the sign here is the mesh's own.
        //
        // CAVEAT for the next person: placed-object triangles come through `expand_objects`, which
        // applies each instance's 4x4 matrix to the VERTICES. A MIRRORED instance (negative-
        // determinant matrix — e.g. a negative scale on one axis) reverses triangle winding, so its
        // faces would come out normal-INVERTED and this filter would read its floors as ceilings.
        // No shipped zone has one today (the build-time winding check below would catch a zone where
        // enough of them existed to matter, and all 34 cached zones pass), but if mirrored instances
        // ever appear, flip `nz` for triangles whose source instance matrix has det < 0 rather than
        // letting the whole zone fall back to facing-blind.
        let tri_nz: Vec<f32> = tris.iter().map(|t| {
            let e1 = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
            let e2 = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
            let n = [e1[1] * e2[2] - e1[2] * e2[1],
                     e1[2] * e2[0] - e1[0] * e2[2],
                     e1[0] * e2[1] - e1[1] * e2[0]];
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            if len > 1e-9 { n[2] / len } else { 0.0 }
        }).collect();

        let cell_size = cell_size.max(1.0);
        if tris.is_empty() || min[0] == f32::MAX {
            return Collision { tris, tri_nz, cells: vec![], origin: [0.0, 0.0], cell_size, cols: 0, rows: 0,
                z_min: 0.0, z_max: 0.0, fallback_hits: Default::default(), tight_plans: Default::default(),
                water: None, from_collision_mesh, zone_line_regions: Vec::new() };
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
        Collision { tris, tri_nz, cells, origin: min, cell_size, cols, rows, z_min, z_max,
            fallback_hits: Default::default(), tight_plans: Default::default(), water: None, from_collision_mesh, zone_line_regions: Vec::new() }
    }

    /// How many times the floor-normal filter's empty-column fallback has fired since zone load, i.e.
    /// how many nav queries have been answered from INVERTED (mis-wound) art rather than from a
    /// properly up-facing floor. `0` = the filter is doing its job everywhere it has been asked.
    /// Non-zero = this zone has mis-wound ground and nav is running degraded there. Reported to the
    /// agent as `nav_degraded` on `/v1/observe/debug` — a degraded mode must never be silent.
    pub fn tight_plans(&self) -> u64 {
        self.tight_plans.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn fallback_hits(&self) -> u64 {
        self.fallback_hits.load(std::sync::atomic::Ordering::Relaxed)
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
    /// The returned point is projected onto the WALKABLE FLOOR beneath the region (#229). A region
    /// point's z is an interior point of the region VOLUME — across the shipped zones it sits 1.5u
    /// to 127u ABOVE the real floor, and it is structurally never a floor height. Navigating to it
    /// verbatim gave A* an unreachable goal tier, so the search could never accept arrival: it
    /// flooded the grid, hit the time cap, and returned a greedy partial route that wedged into a
    /// wall. (halas is the one line whose region z is nearly floor-level — and it is the one line
    /// that always worked.)
    ///
    /// The projection is only taken when the region STILL CONTAINS the projected floor point, so a
    /// tall vertical translocator whose footprint doesn't reach the ground (#266) is never dragged
    /// down off its trigger volume — such a candidate keeps its original z. Candidates are tried
    /// nearest-first, preferring one that projects (a region point hanging over a void is skipped).
    pub fn find_zone_line_near(&self, index: Option<i32>, near: [f32; 3]) -> Option<(i32, [f32; 3])> {
        // Rank by distance to the PROJECTED (standable) point, not the raw region point — and weight
        // vertical separation far above horizontal. A zone line is often baked as several stacked
        // DRNTP leaves over more than one XY; gfaydark's butcher line has a leaf whose only floor is
        // an isolated 74u-high ledge and another that sits on the ground. Ranking on raw 3D distance
        // picked the ledge (it was 28u closer in XY) and sent the char climbing to an unreachable
        // perch. Climbing 74u is not "cheaper" than walking 28u further, so make the cost say so.
        const Z_WEIGHT: f32 = 4.0;
        let cost = |p: &[f32; 3]| (p[0] - near[0]).hypot(p[1] - near[1]) + Z_WEIGHT * (p[2] - near[2]).abs();
        let best_projected = self.zone_line_regions
            .iter()
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .filter_map(|&(idx, p)| self.zone_line_floor_point(idx, p).map(|fp| (idx, fp)))
            .min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1)));
        best_projected.or_else(|| self.zone_line_regions // nothing projects (no floor under any) —
            .iter()                                      // keep the old raw-region-point behaviour
            .filter(|(idx, _)| index.is_none_or(|want| want == *idx))
            .min_by(|a, b| cost(&a.1).total_cmp(&cost(&b.1)))
            .copied())
    }

    /// Project a zone-line region's representative point down onto the walkable floor beneath it.
    /// `None` when there is no floor under it, or when the region does NOT extend down to that floor
    /// (so standing there would not trigger the crossing — see `find_reachable_in_zone_line`, #266).
    fn zone_line_floor_point(&self, index: i32, p: [f32; 3]) -> Option<[f32; 3]> {
        const REGION_DROP: f32 = 400.0;
        let fz = self.floor_beneath(p[0], p[1], p[2], 2.0, REGION_DROP)?;
        // Standing on that floor must still be INSIDE the region — else we'd walk the char to a spot
        // that never fires the auto-cross.
        let inside = self.water.as_ref()
            .and_then(|w| w.zone_line_at(p[0], p[1], fz + 1.0)) == Some(index);
        inside.then_some([p[0], p[1], fz])
    }

    /// Nearest zone-line region whose index is in `in_zone_idxs` (a same-zone destination — an escape
    /// translocator, not a normal neighbour-zone exit) AND whose `DRNTP` footprint is present at the
    /// caller's floor height `near[2]` under the region's XY.
    ///
    /// The auto-cross (`zone_line_at`) is a z-EXACT BSP test. A tall vertical translocator (the Qeynos
    /// guild-vault waterfall) bakes several DRNTP leaves up its column; a leaf near the TOP yields an
    /// interior point high up, and a char that walks to that XY on the vault floor stands BELOW the
    /// leaf and never triggers — so the escape stalls at the portal without teleporting (#266). We keep
    /// only regions still present at the char's own height (`zone_line_at([x, y, near_z]) == idx`), i.e.
    /// whose footprint reaches the floor the char is standing on, so walking there fires the cross.
    /// `None` if no such reachable in-zone portal exists.
    pub fn find_reachable_in_zone_line(&self, in_zone_idxs: &[i32], near: [f32; 3]) -> Option<(i32, [f32; 3])> {
        self.zone_line_regions
            .iter()
            .filter(|(idx, loc)| in_zone_idxs.contains(idx)
                && self.zone_line_at([loc[0], loc[1], near[2]]) == Some(*idx))
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

    /// EVERY surface a vertical column over `[ref_z - down, ref_z + up]` crosses at `(east, north)`,
    /// as `(height, face_normal_z)`, sorted **high → low**. `face_normal_z > 0` = up-facing (a
    /// floor); `< 0` = down-facing (a ceiling / a bridge's underside). Near-vertical walls are
    /// parallel to the ray and never register. Diagnostic/offline-probe entry point and the shared
    /// primitive behind `nearest_floor` / `column_floors`.
    pub fn column_surfaces(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Vec<(f32, f32)> {
        let mut hits = Vec::new();
        self.column_hits(east, north, ref_z, up, down, false, &mut hits);
        hits
    }

    /// Shared vertical-column raycast (Möller–Trumbore). Appends `(hit_z, face_normal_z)` to `out`,
    /// sorted high→low.
    ///
    /// When `floors_only`, returns only **standable** surfaces (`is_standable`, D-2 / #375): FACING-
    /// BLIND, a surface is ground iff `|nz| >= NAV_NEAR_HORIZONTAL` AND it has `NAV_AGENT_HEIGHT` of
    /// open space above it before the next SOLID surface. This replaced the old winding-sign filter
    /// (`nz <= 0`), which deleted real inverted-art floor the character stands on (the qcat live wedge)
    /// and needed a `column_bottom` recovery valve (also removed). The ceiling defence is now HEADROOM
    /// (a ceiling has its roof right above it → fails) plus the caller's `ref_z ± window` (a far roof,
    /// e.g. qcat's 391.8, is simply outside the window). Both `column_hits(true)` (the planner's floor
    /// lookup) and `ground_below` (the walker's clamp) go through this, so the two cannot disagree —
    /// that agreement is the whole point of #375.
    fn column_hits(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32,
                   floors_only: bool, out: &mut Vec<(f32, f32)>) {
        out.clear();
        if self.cols == 0 { return; }
        let z_top = ref_z + up.max(0.0);
        let z_bot = ref_z - down.max(0.0);
        let filter = floors_only;
        // For `is_standable` we need each surface's headroom = distance UP to the next SOLID surface of
        // EITHER winding, which can lie ABOVE the caller's window — so gather facing-blind from `z_bot`
        // up to the zone top, classify, then return only in-window standable surfaces. Same triangles
        // as the window scan (a cell's list is fixed); only the ray is longer, so cost is ~unchanged.
        let gather_top = if filter { self.z_max.max(z_top) + 1.0 } else { z_top };
        let dir_z = z_bot - gather_top; // negative (downward)
        if dir_z.abs() < 1e-6 { return; }
        let eps = 1e-6_f32;
        let cross = |a: [f32; 3], b: [f32; 3]| [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let from = [east, north, gather_top];
        let dir = [0.0, 0.0, dir_z];
        let (c0, c1, r0, r1) = self.cell_range(east, north, east, north);
        // `all` = every surface (both windings) in [z_bot, gather_top]; `out` gets the standable
        // in-window subset. For `filter=false` these coincide (facing-blind, window only).
        let all = out; // reuse the caller's buffer for the raw gather
        for r in r0..=r1 {
            for c in c0..=c1 {
                for &ti in &self.cells[r * self.cols + c] {
                    let nz = self.tri_nz[ti as usize];
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
                    all.push((gather_top + t * dir_z, nz));
                }
            }
        }
        all.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)); // high→low
        if !filter { return; } // facing-blind gather = the window scan; done.

        // Classify each surface as standable and retain only the in-window ones. Headroom is the gap up
        // to the next surface MORE than a slab-thickness above (so the two triangles of one quad floor
        // don't read as each other's ceiling). The topmost surface has open sky above → infinite.
        const SAME_SURFACE: f32 = 0.3;
        let n = all.len();
        let mut keep: Vec<(f32, f32)> = Vec::with_capacity(n);
        for i in 0..n {
            let (z, nz) = all[i];
            if z < z_bot - eps || z > z_top + eps { continue; } // out of the caller's window
            if nz.abs() < NAV_NEAR_HORIZONTAL { continue; }     // too steep to stand on
            // Nearest solid strictly above `z` (indices < i are higher, sorted high→low).
            let mut headroom = f32::INFINITY;
            for j in (0..i).rev() {
                if all[j].0 > z + SAME_SURFACE { headroom = all[j].0 - z; break; }
            }
            if headroom < NAV_AGENT_HEIGHT { continue; }         // under a ceiling — not standing room
            keep.push((z, nz));
        }
        *all = keep;
    }

    /// Find the walkable FLOOR height at `(east, north)` nearest to `ref_z`.
    ///
    /// Casts a vertical column over `[ref_z - down, ref_z + up]`, gathers every UP-FACING triangle
    /// it crosses (a ceiling is not a floor — #329), and returns the one whose height is **closest
    /// to `ref_z`**. This is the surface the player would actually stand on (or step to), and —
    /// unlike a single top-down ray — it does NOT mistake an overhang/awning/bridge ABOVE the floor
    /// for the floor itself. `up` bounds how far you can step UP onto a ledge; `down` how far you
    /// can drop. Returns `None` when no floor exists in the band.
    pub fn nearest_floor(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Option<f32> {
        let mut hits = Vec::new();
        self.column_hits(east, north, ref_z, up, down, true, &mut hits);
        hits.into_iter()
            .map(|(z, _)| z)
            .min_by(|a, b| (a - ref_z).abs().partial_cmp(&(b - ref_z).abs()).unwrap_or(std::cmp::Ordering::Equal))
    }

    /// The highest walkable floor at or below `z` (searching down `down` units, and up `up` units
    /// for a goal that sits a little UNDER the floor it names). This is the "what am I standing
    /// over?" query — used to project a point that lives in a VOLUME (a `DRNTP` zone-line region's
    /// interior point, whose z is a point inside the region solid and is structurally NEVER a floor
    /// height, #229) down onto ground a character can actually stand on.
    pub fn floor_beneath(&self, east: f32, north: f32, z: f32, up: f32, down: f32) -> Option<f32> {
        let mut hits = Vec::new();
        self.column_hits(east, north, z + up.max(0.0), 0.0, up.max(0.0) + down.max(0.0), true, &mut hits);
        hits.first().map(|&(z, _)| z) // high→low ⇒ the first is the highest floor at/below z+up
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

    /// The height of the nearest **standable** surface at/below `origin_z` within `depth`, or `None`.
    /// Native ground clamp uses `origin = foot_z + 1.0`, `depth = 200` (design §3.2).
    ///
    /// D-2 (#375): this is the CONTROLLER's floor clamp, and it now goes through the SAME
    /// `is_standable` predicate as the planner's `column_hits(true)` — so the two cannot disagree about
    /// where the floor is (the qcat wedge was exactly that disagreement: the controller's old
    /// facing-blind first-hit stood on the −42.97 inverted-art walkway while the planner's facing
    /// filter deleted it). It was a single facing-blind ray; it is now the highest standable surface in
    /// `[origin_z − depth, origin_z]`. A surface under a ceiling (headroom `< NAV_AGENT_HEIGHT`) is not
    /// standable — the controller no longer clamps to it (measured against route-success + the faithful
    /// walker corpus for the low-clearance seal risk).
    pub fn ground_below(&self, east: f32, north: f32, origin_z: f32, depth: f32) -> Option<f32> {
        if self.cols == 0 { return None; }
        let mut hits = Vec::new();
        self.column_hits(east, north, origin_z, 0.0, depth.max(0.0), true, &mut hits);
        hits.first().map(|&(z, _)| z) // sorted high→low ⇒ first = highest standable at/below origin_z
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

    /// Is the LINE `from → to` unobstructed? A single centre ray, extended past `to` by `radius`.
    ///
    /// This is a LINE-OF-SIGHT primitive — it answers "can I see/shoot from here to there", NOT
    /// "can the character WALK from here to there". The character is a cylinder, not a line: use
    /// `path_clear` for anything the walker has to physically traverse (#358).
    pub fn line_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let dist = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if dist < 1e-5 { return true; }
        let ext = (dist + radius.max(0.0)) / dist;
        let target = [from[0] + d[0] * ext, from[1] + d[1] * ext, from[2] + d[2] * ext];
        self.nearest_hit_t(from, target).is_none()
    }

    /// The clearance test A* validates ONE GRID EDGE with, at plan resolution `cell`.
    ///
    /// Sweeps the character's collision volume (`path_clear`) on a FINE grid and casts a centre ray
    /// (`line_clear`) on a COARSE one. That asymmetry is deliberate and it is the whole subtlety of
    /// #358 — a cell-centre line is only the line the WALKER will actually walk when the grid is
    /// fine enough:
    ///
    /// * On the FINE local tier (`navigation::LOCAL_CELL` = 2u) the centre line ≈ the walked line,
    ///   and a corridor holds SEVERAL lateral cell choices. Rejecting the one edge that scrapes a
    ///   wall just makes A* pick the cell one over — down the middle. This is the tier the walker
    ///   steers along, and the only tier fine enough to even EXPRESS a route through a gap narrower
    ///   than the character. Measured on the live gfaydark→butcher wedge: the coarse route had 0
    ///   ray/capsule disagreements, the fine route had 2 — on exactly the segments the character
    ///   was stuck on.
    ///
    /// * On the COARSE 8u tier a cell centre only has to have a FLOOR under it — it can sit 0.3u
    ///   from a wall in a corridor the character walks down the middle of without trouble, and a
    ///   corridor narrower than ~16u offers no laterally-adjacent cell to move to. Sweeping the
    ///   volume along that arbitrary lattice line therefore does not reject *unwalkable corridors*,
    ///   it rejects *corridors*. Measured over 1200 start/goal pairs in 10 cached zones: routable
    ///   pairs fell 876 → 813 (−7%), and Ak'Anon — all narrow gnome tunnels — collapsed from 90/120
    ///   to 55/120 (−29%). Sealing a third of a city is a worse bug than the one being fixed (#310
    ///   removed a sub-radius planning fallback for the mirror-image reason), so the coarse tier
    ///   stays a corridor SELECTOR validated by a ray, and the fine tier — re-planned every tick
    ///   ahead of the walker — is what enforces that the volume actually fits.
    pub fn edge_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32, cell: f32) -> bool {
        if cell <= SWEPT_EDGE_MAX_CELL { self.path_clear(from, to, radius) }
        else { self.line_clear(from, to, radius) }
    }

    /// Can the player's COLLISION VOLUME travel from `from` to `to` without crossing geometry?
    ///
    /// The clearance test the planner validates a segment with **must be the same test the
    /// controller moves under** (#358). It was not: this cast a single centre RAY while
    /// `CharacterController` moves a cylinder of `movement::PLAYER_RADIUS`. A corner can be
    /// ray-clear and capsule-blocked — the ray threads the gap, the shoulder does not — so A*
    /// handed the walker routes it is physically incapable of following, and the controller's
    /// slide-along-wall response then shoved it off-route and wedged it.
    ///
    /// The mismatch bites HARDEST on the fine local tier: at `navigation::LOCAL_CELL` = 2u the grid
    /// can actually *express* a route through a sub-capsule gap, where the coarse 8u grid never
    /// could. That is why the overlay showed the coarse line rounding the corner cleanly while the
    /// fine line — the one the walker steers along — hugged the wall. Measured on the live
    /// gfaydark→butcher wedge: coarse route = 0 ray/capsule disagreements, fine route = 2, both on
    /// the segments the character was stuck on.
    ///
    /// So sweep the volume: the centre line plus both SHOULDERS, offset `±radius` perpendicular to
    /// the horizontal motion. This is deliberately the exact feeler pattern `Collision::sweep` (the
    /// mover's own swept-cylinder approximation, design §3.1) uses — the planner and the controller
    /// now share one collision model, so "the planner says clear" and "the controller can traverse
    /// it" cannot drift apart. Matching an *ideal* capsule instead would be a different (and
    /// weaker) guarantee: it would agree with geometry the controller does not actually implement.
    ///
    /// Cost is 3 rays instead of 1. The A* edge test runs two of these (chest + feet) per edge; the
    /// grid broad-phase keeps each ray to a handful of triangles.
    ///
    /// Returns `true` (clear) when there is no zone geometry loaded.
    pub fn path_clear(&self, from: [f32; 3], to: [f32; 3], radius: f32) -> bool {
        let d = [to[0] - from[0], to[1] - from[1]];
        let hlen = (d[0] * d[0] + d[1] * d[1]).sqrt();
        let r = radius.max(0.0);
        // Purely vertical (or zero-length) motion has no horizontal shoulders to sweep.
        if hlen < 1e-5 || r < 1e-5 { return self.line_clear(from, to, radius); }
        let perp = [-d[1] / hlen * r, d[0] / hlen * r];
        // Feelers ACROSS THE WHOLE DIAMETER, not just the two shoulders. Three rays (centre + both
        // shoulders) leak on DIAGONAL motion: the shoulders are offset perpendicular to the travel
        // direction, so against a wall the segment crosses at an angle, one shoulder starts already
        // PAST the wall plane and the other never reaches it — both slide along the wall instead of
        // across it, and the capsule threads a slot narrower than itself. Caught by mutation-testing
        // this very fix: A* diagonally threaded a 2.5u slot with a 2.0u clearance, on an edge every
        // one of the three rays called clear.
        //
        // Sampling the diameter at r/2 spacing closes it: a wall panel that ENDS inside the swept
        // rectangle now has a feeler either side of its end. This is an approximation of a swept
        // disc, not an exact one — a needle thinner than r/2 threading between feelers would still
        // slip — but zone geometry is walls and panels, not needles, and it is the same family of
        // approximation the mover itself uses.
        //
        // LIMIT (#381): the feelers are offset PERPENDICULAR to travel, so they cannot see a wall the
        // segment runs ALONGSIDE — a ray parallel to a plane never intersects it. A segment skimming
        // a wall within the body radius, but never crossing it, still reads clear at every feeler.
        // This is pre-existing (the single centre ray was blind to it too) and it is the floor this
        // fix cannot get below: measured end-to-end, fine waypoints whose capsule does not fit drop
        // 0.657% -> 0.248% across five zones, but do not reach zero (akanon 0.65%, blackburrow
        // 0.84%). Adding feelers cannot close it — no finite set of parallel rays can. The durable
        // answer is a baked clearance field (#372/#378), not more rays. Do not "fix" it here.
        const FEELERS: [f32; 5] = [-1.0, -0.5, 0.0, 0.5, 1.0];
        FEELERS.iter().all(|&f| self.line_clear(
            [from[0] + perp[0] * f, from[1] + perp[1] * f, from[2]],
            [to[0] + perp[0] * f, to[1] + perp[1] * f, to[2]],
            radius,
        ))
    }

    /// Does the character have `clearance` of walkable GROUND all around `(east, north)` at `z`?
    ///
    /// The other hazard. `edge_clear`/`path_clear` sweep the character's volume against geometry
    /// that is IN THE WAY — walls. Nothing in the search saw geometry that is MISSING: a cliff lip,
    /// the edge of a bridge, a dock, a waterline. A route may therefore be perfectly wall-clear and
    /// still run along the very brink of a drop, and the walker — which slides on contact and gets
    /// shoved around by server position corrections — eventually goes over it.
    ///
    /// Probes the four axial directions at `clearance` (the same predicate and the same ±band as the
    /// waypoint inset's `edge_ok`, so the search and the inset agree about what an "edge" is) and
    /// requires ground within a tight vertical band of `z` at each. A drop below, a wall lip above,
    /// and open water all read as "no ground" and so as an edge to keep away from.
    pub fn ground_margin_ok(&self, east: f32, north: f32, z: f32, clearance: f32) -> bool {
        if clearance <= 0.0 { return true; }
        [(clearance, 0.0), (-clearance, 0.0), (0.0, clearance), (0.0, -clearance)]
            .iter()
            .all(|&(dx, dy)| self.nearest_floor(east + dx, north + dy, z, 3.0, 8.0)
                .is_some_and(|f| (f - z).abs() <= 8.0))
    }

    /// ALL distinct walkable surface heights at `(east, north)` within `[ref_z - down, ref_z + up]`,
    /// sorted high→low with near-duplicates (within 1u) merged. Unlike `nearest_floor` (one surface),
    /// this exposes every floor in the column — essential for multi-level pathfinding where a ramp or
    /// lower floor sits UNDER an upper ledge (so A* can choose to descend instead of always snapping
    /// to the nearest/upper surface).
    pub fn column_floors(&self, east: f32, north: f32, ref_z: f32, up: f32, down: f32) -> Vec<f32> {
        let mut hits = Vec::new();
        self.column_hits(east, north, ref_z, up, down, true, &mut hits);
        let mut zs: Vec<f32> = hits.into_iter().map(|(z, _)| z).collect(); // already high→low
        zs.dedup_by(|a, b| (*a - *b).abs() < 1.0);
        zs
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
        self.find_path_res(start, goal, radius, avoid, allow_partial, 8.0, None, 0.0, PlanCtx::default())
    }

    /// The goal's floor when the caller's `z` could NOT be resolved to a tier — i.e. it sits below
    /// every floor in the goal's column (an agent passing a rough `z`, usually 0, or a map coord).
    /// Returns the nearest floor anywhere in that column: the goal the caller meant.
    ///
    /// `Some(z)` here means **the client is about to change the caller's goal**. That is an
    /// accommodation, and an accommodation presented as compliance is a lie — so it is reported, not
    /// quietly performed: the planner surfaces it as `nav_reason: goal_z_snapped` and says so in the
    /// message log, rather than letting an agent that asked for `z: 0` be told `arrived` at `z: 47`
    /// without ever learning its goal was moved.
    ///
    /// `None` = there is no floor anywhere in the column: the goal is off the mesh, and that is a
    /// genuine `GoalNotWalkable` (fail loudly, don't search).
    pub fn snap_goal_to_column_floor(&self, goal: [f32; 3]) -> Option<f32> {
        const COLUMN: f32 = 1000.0; // the whole column: "is there ANY floor at this XY?"
        self.nearest_floor(goal[0], goal[1], goal[2], COLUMN, COLUMN)
    }

    /// Did resolving `goal` require SNAPPING its z to a different floor? `Some(floor_z)` = yes, and
    /// the caller's goal is being changed — see [`Collision::snap_goal_to_column_floor`]. Used by the
    /// planner to tell the agent, so the snap is never silent.
    pub fn goal_z_was_snapped(&self, goal: [f32; 3]) -> Option<f32> {
        const GOAL_TIER_TOL: f32 = 8.0;
        const GOAL_DROP: f32 = 400.0;
        let resolved = self.nearest_floor(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_TIER_TOL)
            .or_else(|| self.floor_beneath(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_DROP));
        // A tier the caller named was honoured → nothing was changed → nothing to report.
        if resolved.is_some() { return None; }
        self.snap_goal_to_column_floor(goal)
    }

    /// BEST-EFFORT route at an arbitrary grid resolution `cell`, optionally bounded to `max_search`
    /// units of the start (so a FINE plan stays local + cheap even if it hits an obstacle).
    /// `cell` = 8.0 + `max_search` = None reproduces the classic whole-zone nav grid.
    ///
    /// This returns "the best waypoints I have" — a complete route, or (with `allow_partial`) a
    /// partial one toward the frontier — and CANNOT say why it has none. It is for LOCAL STEERING
    /// (the fine 2u tier, whose partials are a 40u steering hint the walker re-plans every tick),
    /// never for answering an agent's "can I get there?". For that, use [`Collision::find_path_ex`],
    /// which distinguishes "no route" from "I gave up" (#337/#356).
    #[allow(clippy::too_many_arguments)]
    pub fn find_path_res(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        allow_partial: bool, cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> Option<Vec<[f32; 3]>> {
        // Plan owner: materialise the plan-wide budget so this call's internal A* passes share one cap.
        let ctx = ctx.ensure_budget();
        let (s, _tight) = self.search_tiered(start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx);
        match s.path {
            Some((p, true)) => Some(p),
            Some((p, false)) if allow_partial => Some(p),
            _ => None,
        }
    }

    /// The HONEST plan (#337, #356): run A* and report which of the three genuinely-different
    /// answers came back — a complete `Route`, a definitive `Unreachable`, or an `Exhausted` search
    /// that hit a limit and therefore does not know.
    ///
    /// There is no `allow_partial` flag: a partial route is not an answer to "route me to the goal",
    /// it is a consolation prize, so it can only ever ride along inside `Exhausted` — and only when
    /// it makes real progress (`PARTIAL_MIN_UNITS`). A search that CLOSES its frontier without
    /// reaching the goal now returns `Unreachable` and NO waypoints at all: walking a partial in
    /// that case is exactly the silent wedge of #337.
    #[allow(clippy::too_many_arguments)]
    pub fn find_path_ex(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> PlanOutcome {
        // Plan owner (when called standalone). When `plan_path` drives this, `ctx` already carries a
        // shared counter, so `ensure_budget` is a no-op and all 13 calls keep sharing the one budget.
        let ctx = ctx.ensure_budget();
        let (s, _tight) = self.search_tiered(start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx);
        match s.path {
            Some((p, true)) => PlanOutcome::Route(p),
            other => match s.limit {
                // Cut short → "I don't know". Hand back the partial ONLY if it makes genuine
                // goal-ward progress, so the walker can advance a stage and re-plan from there.
                Some(limit) => {
                    let progress = other
                        .filter(|_| s.progress >= PARTIAL_MIN_UNITS)
                        .map(|(p, _)| p);
                    PlanOutcome::Exhausted { limit, progress }
                }
                // Frontier CLOSED → a definitive no. No partial: see the doc comment.
                None => PlanOutcome::Unreachable(s.no_route.unwrap_or(NoRoute::SearchClosed)),
            },
        }
    }

    /// The FINE LOCAL STEERING plan (#382): a bounded A* at `cell` resolution (2 u) within `bound`
    /// units (40 u) of the character, aimed at a carrot on the committed coarse route.
    ///
    /// # It runs to COMPLETION. There is no wall clock.
    ///
    /// `PlanCtx::default()` arms **no deadline**, and that is the point of #382. This search used to
    /// run inline on the network thread under a 150 ms budget — a residual net-thread stall of the
    /// same class that caused the #257/#302 linkdead bugs (measured, release, akanon: mean 15.3 ms,
    /// worst **358 ms**), and, worse, a budget that made its answer unfalsifiable: a search that ran
    /// out of clock and one that proved the corridor impassable came back as the same short
    /// `Option<Vec<_>>`. It now runs on `nav_planner::LocalPlanner`, where nothing real-time waits on
    /// it, so it can afford the truth.
    ///
    /// Termination is **spatial, not temporal**: `max_search = Some(bound)` confines the frontier to a
    /// 40 u disc at 2 u cells — a few thousand cells — so the search genuinely closes. `MAX_NODES`
    /// remains as an absolute backstop, and hitting it yields [`LocalOutcome::Exhausted`]: a
    /// *distinguishable* "I stopped looking", never a silent "there is no way through". A consequence
    /// worth stating plainly: with the clock gone, this tier is **deterministic** — the same character
    /// in the same spot now steers the same way whatever else the box is doing.
    ///
    /// Clearance: always the MINIMUM (`PLAYER_RADIUS`). See `search_tiered` — a bounded plan does not
    /// choose a route, it threads one the coarse planner already chose with room, so the generous pass
    /// buys nothing and costs a second search.
    /// `carrot_tol` is how near the carrot counts as REACHING it. This is not slop — it is the
    /// question. A carrot is an interpolated point on a line between two 8 u COARSE cell centres, so
    /// its z is a coarse floor height and its XY routinely lands a couple of units off the fine grid's
    /// walkable floor (or inside a wall corner the 8 u grid cut). A* would then never accept arrival at
    /// the goal *cell*, and a plan that gets the walker exactly where it needs to go would be reported
    /// as "there is no way through" — which is not merely pessimistic, it would arm a spurious coarse
    /// re-plan (#246) and publish a false `nav_local`. Measured: judging on A*'s strict goal-cell flag
    /// instead of this tolerance loses **16 of 1447** real carrots across the zone corpus.
    ///
    /// So the fine tier's success criterion is the walker's: *did the plan get me to the carrot?* —
    /// the same test the walker itself applied before this change (`LOCAL_CELL * 2`).
    pub fn find_path_local(&self, start: [f32; 3], goal: [f32; 3], cell: f32, bound: f32, carrot_tol: f32)
        -> LocalOutcome
    {
        // Plan owner: the fine tier's node cap, with a shared plan-wide counter so its two anchor
        // searches (char + cell-centre retry) draw from one budget (#394). The cap is a runaway
        // backstop; the 40u `bound` is what really terminates this search.
        let ctx = PlanCtx { node_cap: Some(NET_TIER_NODE_CAP), ..PlanCtx::default() }.ensure_budget();
        let (s, _tight) = self.search_tiered(
            start, goal, crate::movement::PLAYER_RADIUS, &[], cell, Some(bound), 0.0, ctx);
        // The partial rides along in EVERY variant — it is a steering hint, not a route proposal.
        let steer: Vec<[f32; 3]> = s.path.map(|(p, _)| p).unwrap_or_default();
        let reached = steer.last()
            .is_some_and(|w| (w[0] - goal[0]).hypot(w[1] - goal[1]) <= carrot_tol);
        if reached { return LocalOutcome::Threaded(steer); }
        match s.limit {
            // Cut short → "I don't know". Never "the corridor is blocked".
            Some(limit) => LocalOutcome::Exhausted { limit, steer },
            // Frontier CLOSED inside the window → a falsifiable local "no way through".
            None => LocalOutcome::NoWayThrough { steer, why: s.no_route.unwrap_or(NoRoute::SearchClosed) },
        }
    }

    /// TIERED CLEARANCE (#358). Search at a GENEROUS clearance (`NAV_PREFERRED_CLEARANCE`) and fall
    /// back to the MINIMUM one — exactly `movement::PLAYER_RADIUS` — only when no generous route
    /// exists. Returns the answering search plus `tight`: the route only exists at the minimum, i.e.
    /// it threads a narrow door or a tight bridge with no margin to spare.
    ///
    /// Why default above the character's own size: fitting is not walking. A route planned at exactly
    /// `PLAYER_RADIUS` may skim a wall, a cliff lip or the edge of a bridge with zero margin, and the
    /// walker — which slides on contact and is shoved around by server position corrections — falls
    /// off it. Normal walking should have room.
    ///
    /// **The floor is `PLAYER_RADIUS` and it is not negotiable.** #310 removed a fallback that planned
    /// at 0.5x and 0.25x `PLAYER_RADIUS`, threading gaps narrower than the character's real collision
    /// radius and handing the walker routes it could not fit through. This is the mirror image, and the
    /// distinction is the whole design: the DEFAULT is above the radius, the FALLBACK is AT it, never
    /// under. If no route exists even at `PLAYER_RADIUS`, the honest answer is no route.
    ///
    /// **The generous tier is strictly BEST-EFFORT and can never starve the minimum tier.** The two
    /// searches share ONE budget — the caller's — and the generous pass gets a slice of it, never a
    /// budget of its own. Arming a fresh deadline per pass is how a plan quietly costs two budgets,
    /// which on the net-thread local tier is the #302 stall disease; `PlanCtx` exists precisely so a
    /// plan is bounded by one deadline no matter how many A* calls it makes. The deadline is
    /// materialised ONCE, up front, so the split cannot drift as the clock runs.
    ///
    /// Only a COMPLETE generous route is accepted. A generous partial is not evidence the goal is
    /// unreachable, only that it is unreachable *with room* — and the honest `Unreachable` /
    /// `Exhausted` verdict (#337/#356) must always come from the minimum tier, which is the one that
    /// knows whether a route exists at all.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn search_tiered_for_test(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> (Search, bool) {
        self.search_tiered(start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx)
    }

    #[allow(clippy::too_many_arguments)]
    fn search_tiered(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> (Search, bool) {
        // The hard floor (#310). Never search below the radius the controller actually moves with.
        let minimum = radius.max(crate::movement::PLAYER_RADIUS);
        // Tiering is a ROUTE-CHOICE mechanism, and only a plan that actually chooses a route should
        // pay for it. A BOUNDED plan (`max_search: Some`) is the fine local tier: it follows a carrot
        // on a coarse route that was ALREADY chosen with room, inside a 40u window where there is no
        // meaningful alternative to choose. Asking it for a roomy route buys nothing and costs a
        // second search, every nav tick. Measured on the production call (2u cell / 40u bound): the
        // second pass adds ~30-60% mean on top of the sweep and DOUBLES the plans that overrun a
        // 150 ms budget (blackburrow 17 -> 30 of 240). So the local tier plans at the MINIMUM
        // clearance and spends its time on the question it exists to answer — does the character FIT
        // — while the coarse planner picks the roomy route. (Both tiers are off the net thread now:
        // the coarse one since #377, the fine one since #382.)
        let chooses_a_route = max_search.is_none();
        let preferred = if chooses_a_route { minimum.max(NAV_PREFERRED_CLEARANCE) } else { minimum };
        if preferred > minimum {
            // Give the generous pass a SLICE of the budget that REMAINS at this point in the plan, and
            // let it draw from the same plan-wide counter (`..ctx.clone()` shares the `Arc`). Since both
            // passes share one running total, the minimum pass — which gets the FULL cap — always has
            // whatever the generous pass did not spend, so the roomy tier can never starve the tier that
            // actually decides whether a route exists (#302). The slice is computed on the budget LEFT,
            // not the original cap, exactly as `main`'s `generous_deadline` sliced the remaining time.
            let cap = ctx.node_cap.unwrap_or(MAX_NODES).min(MAX_NODES);
            let used = ctx.expanded.as_ref().map(|a| a.load(std::sync::atomic::Ordering::Relaxed)).unwrap_or(0);
            let generous_cap = used + generous_node_cap(Some(cap.saturating_sub(used))).unwrap_or(0);
            let generous_ctx = PlanCtx { node_cap: Some(generous_cap), ..ctx.clone() };
            let s = self.search(start, goal, preferred, avoid, cell, max_search, aggro_buffer, generous_ctx);
            if matches!(s.path, Some((_, true))) { return (s, false); }
        }
        let s = self.search(start, goal, minimum, avoid, cell, max_search, aggro_buffer, ctx);
        let tight = preferred > minimum && matches!(s.path, Some((_, true)));
        // Honesty: a tight route is a RISKIER route — no margin from the walls and drops it passes.
        // An agent must be able to tell it is walking one, so count it; `/v1/observe/debug` reports
        // it as `nav_tight`. A degraded mode must never be silent.
        if tight { self.tight_plans.fetch_add(1, std::sync::atomic::Ordering::Relaxed); }
        (s, tight)
    }

    /// One logical A* run: the char-anchored search, with the cell-centre-anchored retry for a
    /// boxed-in start folded in.
    ///
    /// Anchor A* at the character's true position (so its first leg is one the walker can actually
    /// take). But if the character's EXACT spot is BOXED IN — standing inside a tree trunk's or a
    /// wall's footprint, where the rays out of it are blocked or lead into a sealed pocket — that
    /// anchor strands the search in a handful of nodes. That is precisely the isolated start the
    /// cell-centre anchor + start-cell hop exist to rescue (#2/#205), so fall back to them.
    ///
    /// The retry is gated on the failed search having explored almost NOTHING, so it fires only for
    /// a boxed-in start (a few nodes, microseconds) and never doubles the cost of a genuine long
    /// search that failed on its merits.
    #[allow(clippy::too_many_arguments)]
    fn search(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx) -> Search {
        const BOXED_IN_NODES: usize = 64;
        // Is this search's verdict really "the goal is unreachable", or is it "I couldn't get OUT of
        // where the character is standing"? The two look identical from the outside — both close the
        // frontier without reaching the goal — and telling them apart is the whole job here.
        //
        // The tell is the SIZE of the component the search closed. A search that explored a handful
        // of cells did not survey the zone and rule the goal out; it never left the doorstep. The
        // character is boxed in (stood inside a tree trunk, wedged on a slope face) and the fix is to
        // re-anchor the START (#205) — not to tell the agent "there is no route", which would be a
        // FALSE definitive no, the single worst thing this planner can say.
        //
        // Note this deliberately ignores any PARTIAL route the search dribbled out. Live gfaydark:
        // the char wedged on terrain, A* closed after 1 node, the cell-centre retry crawled 5 nodes
        // and produced a 2-cell partial — and an earlier version of this function took the existence
        // of that stub as proof the search had really surveyed the zone, and reported `search_closed`
        // on a goal that was perfectly reachable from 16u away. A 2-cell stub is not a survey. Only a
        // COMPLETE route (`reached`) proves the search got anywhere.
        let boxed_in = |s: &Search| {
            s.limit.is_none()
                && s.closed_n < BOXED_IN_NODES
                && !s.path.as_ref().is_some_and(|(_, reached)| *reached)
                && s.no_route != Some(NoRoute::GoalNotWalkable)
                && s.no_route != Some(NoRoute::NoGeometry)
        };
        // Both anchors share the plan-wide budget (same `ctx.expanded` Arc): the retry draws down what
        // the first anchor left, so the two together cost one budget, not two.
        let s = self.astar(start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx.clone(), true);
        if !boxed_in(&s) { return s; }
        // Anchoring at the character's exact position got nowhere. Retry from the cell centre (the
        // classic #2/#205 rescue) before believing anything.
        let mut retry = self.astar(start, goal, radius, avoid, cell, max_search, aggro_buffer, ctx, false);
        // Never LOSE a partial route by retrying. If the char-anchored search produced one and the
        // cell-centre retry produced nothing, keep the one we had: the fine local tier steers on it,
        // and dropping it is the same steering-starvation that stopped the halas swimmer — just
        // reached through the other anchor (#377 review, N1).
        if retry.path.is_none() && s.path.is_some() {
            retry = Search { path: s.path, ..retry };
        }
        if boxed_in(&retry) {
            // Both anchors are sealed in: the START is the problem, not the goal. Name it, so
            // `plan_path` re-anchors (#205) and `find_path_ex` reports `start_isolated` rather than
            // the false "no route to the goal" this used to collapse into.
            //
            // The partial route is deliberately KEPT here. `find_path_ex` already drops it on the
            // `Unreachable` path (an honest "no" carries no waypoints), so wiping it here bought
            // nothing — but it also starved `find_path_res`, the FINE LOCAL STEERING tier, which
            // legitimately runs tiny bounded searches whose component is *always* small and which
            // needs its partial as a steering hint. Live halas: a swimmer floating at the water's
            // edge has exactly such a search, and with the partial wiped the walker stopped swimming
            // and wedged at the shoreline while the coarse planner cheerfully re-issued a perfect
            // 78-waypoint route across the water, every tick, for 8 attempts.
            return Search { no_route: Some(NoRoute::StartIsolated), ..retry };
        }
        retry
    }

    /// The A* itself. `prefer_char_anchor` expands the START node from the character's own (x, y)
    /// rather than its cell centre (see `anchor_at_char`).
    ///
    /// It reports everything the honest-outcome layer needs to tell "no route exists" from "I gave
    /// up": whether the frontier was CLOSED or the search was cut short by a limit, how many nodes
    /// it closed (a handful = a boxed-in start), and how much ground a partial route actually
    /// covers. The old version returned a bare `Option`, which is why a timeout and a genuine
    /// no-route were indistinguishable for months (#337/#356).
    #[allow(clippy::too_many_arguments)]
    fn astar(&self, start: [f32; 3], goal: [f32; 3], radius: f32, avoid: &[[f32; 2]],
        cell: f32, max_search: Option<f32>, aggro_buffer: f32, ctx: PlanCtx,
        prefer_char_anchor: bool) -> Search {
        use std::collections::BinaryHeap;
        use std::cmp::Ordering;
        if self.cols == 0 || self.rows == 0 { return Search::no_route(NoRoute::NoGeometry); }
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
        // Same cell as the goal: a straight walk. Still start the route AT the character (see the
        // `path.insert(0, ...)` note at the end of the search) so pure pursuit steers along it.
        if (sc, sr) == (gc, gr) {
            return Search {
                path: Some((vec![[start[0], start[1], start[2]], [goal[0], goal[1], goal[2]]], true)),
                ..Default::default()
            };
        }
        const GOAL_TIER_TOL: f32 = 8.0; // reached floor within this of goal_floor == the right tier
        // The goal's TIER: the walkable surface at the goal XY the caller means. On a zone with
        // stacked levels (neriakc, a walkway over a lower floor) the goal cell exists at several
        // heights; A* must finish on the one the caller asked for, else it routes the whole approach
        // along the wrong tier and the walker stalls / lands a level off (#35).
        //
        // The goal z is NOT always a floor height, and assuming it is was the #229 wedge: a
        // `zone_cross` aims at a `DRNTP` region's representative point, whose z is an interior point
        // of the region VOLUME — measured 1.5u to 127u ABOVE the real floor on every zone line in
        // the shipped assets. Snapping that to the "nearest surface within ±20" produced a PHANTOM
        // tier (or, pre-normal-filter, a ceiling), and then GOAL_TIER_TOL rejected every cell A*
        // reached at the TRUE floor — so A* could never accept arrival, flooded the grid, timed out,
        // and returned a greedy partial that wedged into a wall.
        //
        // So: snap to a floor only when the goal z really IS one (within GOAL_TIER_TOL of a
        // surface). Otherwise the goal is a point in the air / in a volume — project it DOWN onto
        // the walkable floor beneath it, however far below that is.
        const GOAL_DROP: f32 = 400.0; // a volume point can sit far above its floor
        // The snap window is GOAL_TIER_TOL (8), deliberately NARROWER than the old +/-STEP_UP (20).
        // It has to be: the two clauses below disagree about what a goal that is 8..20u ABOVE a floor
        // means, and only one of them can win. The old +/-20 said "that's still this floor" — which is
        // exactly how a zone-line region point 12.9u above its floor (gfaydark->felwithea) got snapped
        // onto a phantom tier. Tying the window to the SAME tolerance A* later uses to accept arrival
        // (GOAL_TIER_TOL) makes the two agree by construction: if the goal z is within tier tolerance
        // of a real floor it IS that tier; otherwise it is a point in the air and gets projected down.
        // Cost: a goal reported 8..20u BELOW its floor now resolves to the floor beneath it instead of
        // stepping up to the one above. That is the rarer and more conservative error (walk to the
        // ground under the target, not to a tier the caller never named), and callers that report a z
        // that far under their own floor are already lying to us.
        let resolved_goal_floor = self.nearest_floor(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_TIER_TOL)
            .or_else(|| self.floor_beneath(goal[0], goal[1], goal[2], GOAL_TIER_TOL, GOAL_DROP));
        // AN UNACCEPTABLE GOAL FAILS IMMEDIATELY AND LOUDLY (#337). If there is no walkable floor at
        // or beneath the goal, no cell A* can ever reach will satisfy the arrival test — the search
        // is guaranteed to flood the entire grid and come back with a greedy partial that the walker
        // drives into a wall. That is not a search problem, it is an invalid question, and answering
        // it with a 2-second flood and a wedge is the exact dishonesty this issue is about. Say so
        // now, in microseconds, with a reason the agent can act on.
        //
        // EXCEPT for a zone-line goal (`goal_region`): arrival there is decided by "am I standing
        // INSIDE the region volume?", not by the goal cell's floor tier — a region's representative
        // point is an interior point of a VOLUME whose z is structurally never a floor height (#229).
        // Its walkability is not ours to judge here, so let the search answer it.
        //
        // A SLOPPY Z IS NOT AN UNREACHABLE GOAL. Agents routinely pass a rough z (often 0, or a map
        // coordinate), and the goal's real floor can sit well above it — `floor_beneath` only looks
        // DOWN, so it finds nothing. An earlier version hard-failed those as `goal_not_walkable`,
        // which is a FALSE definitive no: the XY is perfectly walkable and `main` routed to it fine
        // (its wrong-tier `goal_fallback` accepted the goal cell at whatever tier it really had).
        // Live North Qeynos: `goto (-40,250,z=0)` refused to move at all. So before giving up, snap
        // the goal to the nearest floor ANYWHERE in its column — that is the goal the caller meant.
        let goal_floor = match resolved_goal_floor
            .or_else(|| self.snap_goal_to_column_floor(goal))
        {
            Some(f) => f,
            // NO floor anywhere in the goal's column: off the mesh, or out over a void. THAT is an
            // unacceptable goal, and it fails immediately and loudly — no flooding the grid for
            // seconds and handing back a stub to wedge on (#337).
            None if ctx.goal_region.is_none() => {
                tracing::info!("find_path: goal ({:.0},{:.0},{:.1}) has NO walkable floor anywhere in its column \
                    — unreachable by construction (not searching)", goal[0], goal[1], goal[2]);
                return Search::no_route(NoRoute::GoalNotWalkable);
            }
            None => goal[2],
        };
        // Start floor: anchor to the caller's EXACT (x,y), NOT the 8u cell center. Near a wall the
        // cell center can fall on the wall's footprint, whose only surface is the wall-TOP — so the
        // center probe would start the char up on the wall and route the whole path along it, a
        // height the walker can't scale from a standstill (it wedges → 0 progress, #2). The exact
        // start point sits on the real floor (e.g. the street) the char is actually standing on.
        // The caller's z can still be stale, so try several reference levels; fall back to the cell
        // center only if the exact point has no floor at any of them.
        //
        // WATER (#329, #197p2): a character FLOATING in water has no floor under it in any
        // meaningful sense — its support is the WATER SURFACE, and buoyancy holds it there. Anchoring
        // it to a slab instead planned routes it physically cannot follow: in the flooded qcat spawn
        // corridor the nearest surface to a floater was the CEILING (the water line is flush with
        // it), so A* planned across the ceiling plane; in the Halas pool the nearest surface was the
        // pool BOTTOM 128u down, so A* planned along the bottom and the swimmer dived and stranded.
        // If the char is in water with no footing directly beneath it, anchor A* to the surface it is
        // actually floating on — which is exactly the tier the WATER SURFACE TRAVERSAL edges connect.
        const FOOTING: f32 = 4.0; // a floor this close under the feet = standing (wading), not floating
        let floating_surface = self.water.as_ref().and_then(|w| {
            let (x, y, z) = (start[0], start[1], start[2]);
            let wet = w.is_water(x, y, z) || w.is_water(x, y, z - 1.0);
            let footed = self.nearest_floor(x, y, z, 2.0, FOOTING).is_some();
            if wet && !footed { w.surface_z(x, y, z - 1.0) } else { None }
        });
        // The floor under the character's EXACT position (not its cell centre). When this resolves,
        // A* is anchored geometrically AT THE CHARACTER (see `anchor_at_char` below) and needs no
        // cell-centre fallback at all.
        let exact_start_floor = floating_surface
            .or_else(|| [start[2], goal[2], 0.0, -60.0, -120.0]
                .into_iter()
                .find_map(|rz| self.nearest_floor(start[0], start[1], rz, STEP_UP, MAX_DROP)));
        // TRUE-POSITION START ANCHOR (#229's last mile). A* expands each node from its CELL CENTRE,
        // including the start — but the walker drives from where the character actually STANDS. When
        // the character is pressed against a wall, its own 8u cell centre can lie INSIDE the wall's
        // footprint; the start cell then gets hopped to a neighbour (below), A* plans a perfectly
        // valid chain from THAT centre, and the walker charges from its real position straight at the
        // first waypoint — through the wall in between. Live everfrost→blackburrow: a 99-waypoint
        // route in which every A*-validated cell-centre segment was clear and ONLY the
        // character→waypoint[0] leg was blocked; the walker pressed into the wall, made no progress,
        // re-planned the identical route, and stalled out after 8 attempts.
        //
        // So when the character's exact position has a floor, expand the START node from THE
        // CHARACTER'S OWN (x, y) instead of the cell centre. Every first-hop edge is then clearance-
        // tested from where the walker really is, so the route's first leg is always one it can
        // actually take — and the cell-centre-in-a-wall hop becomes unnecessary (the start node's
        // floor is the character's real floor, never the wall-top the hop existed to dodge).
        let anchor_at_char = prefer_char_anchor && exact_start_floor.is_some();
        let start_floor = exact_start_floor
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
        // When the start anchor is a WATER SURFACE (a floating character), there is no solid floor at
        // that height by definition — the cell is valid if it is swimmable water there instead.
        //
        // Only needed when we could NOT anchor at the character's exact position: with
        // `anchor_at_char` the start node's floor is the character's real floor (never the wall-top)
        // and its edges are cast from the character's real (x, y), so hopping the cell would only
        // move the plan's origin away from the walker — which is the very bug it used to cause.
        let cell_has_start_floor = |c: i32, r: i32| -> bool {
            let ctr = center(c, r);
            if floating_surface.is_some() {
                return self.water.as_ref().is_some_and(|w| w.is_water(ctr[0], ctr[1], start_floor - 1.0));
            }
            self.column_floors(ctr[0], ctr[1], start_floor, STEP_H, MAX_STEP_DOWN)
                .into_iter().any(|z| (z - start_floor).abs() <= GOAL_TIER_TOL)
        };
        let (sc, sr) = if anchor_at_char || cell_has_start_floor(sc, sr) {
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
        // The node cap is the ONLY bound on a search, so it also decides whether the definitive verdict
        // `Unreachable(SearchClosed)` can ever be reached. Too tight and a whole-zone flood hits the cap
        // and downgrades to `Exhausted` ("I don't know") even when the frontier really was closable — a
        // false "I don't know" in place of an honest "no". So it is chosen by MEASUREMENT, above.
        //
        // The module-level `MAX_NODES` (8M, chosen so everfrost's 1.12M-node whole-zone close still
        // reaches SearchClosed, #394) is the ABSOLUTE backstop; a caller may set a TIGHTER `ctx.node_cap`
        // (the fine tier does). It is a NODE COUNT, not a wall clock: whichever cap bites, the same query
        // hits it after the same number of expansions on every machine, so the outcome is reproducible.
        // A wall-clock budget used to sit here too and made the answer machine-speed-dependent — deleted.
        let node_cap = ctx.node_cap.unwrap_or(MAX_NODES).min(MAX_NODES);
        let mut limit: Option<PlanLimit> = None;
        // How much walkable ground the route must keep around it (the LEDGE margin, enforced on the
        // neighbour cell below). Asked for ONLY above the minimum clearance: at `PLAYER_RADIUS` the
        // plan promises exactly "the character fits" — which is the promise that keeps a narrow
        // bridge, gangplank or catwalk routable at all — and the GENEROUS tier layers standing room
        // on top of it (`search_tiered`). A swim plan is exempt outright: a floating character has no
        // ground under it by definition, so the probe would reject every cell it must cross.
        let ledge_margin = if radius > crate::movement::PLAYER_RADIUS && floating_surface.is_none() {
            radius
        } else {
            0.0
        };
        let mut margin_ok: std::collections::HashMap<(i32, i32, i32), bool> = std::collections::HashMap::new();
        // Aggro-avoidance (#67): softly bias A* AWAY from cells near NPCs so long routes skirt mob
        // camps instead of plowing through them and getting the player killed. Proactive (before
        // aggro) and faction-agnostic — the client has no broad faction data, so it avoids ALL
        // nearby NPCs; the penalty is MILD and fades to 0 at AGGRO_RADIUS, so a route is only
        // nudged around a camp when a clear alternative exists — it never becomes "no route".
        // `aggro_buffer` (#242) WIDENS that radius when the caller asks to route more conservatively
        // around hostile pulls (`avoid_aggro` on /v1/move/*), and scales the penalty up with it so a
        // bigger buffer gives real berth — while staying soft (still fades to 0 at the edge, so an
        // unavoidable disc is threaded at shortest exposure rather than failing the route).
        let aggro_radius  = 50.0 + aggro_buffer.max(0.0);       // ~a low-level mob's aggro range + buffer
        let aggro_penalty = 60.0 * (1.0 + aggro_buffer.max(0.0) / 50.0); // firmer with a wider buffer
        let aggro_cost = |x: f32, y: f32| -> f32 {
            let mut worst = 0.0f32;
            for p in avoid {
                let d2 = (x - p[0]) * (x - p[0]) + (y - p[1]) * (y - p[1]);
                if d2 < aggro_radius * aggro_radius {
                    worst = worst.max(aggro_penalty * (1.0 - d2.sqrt() / aggro_radius));
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
        // The PLAN-WIDE running total of expansions (#394 review). `astar` increments the shared
        // counter (materialised by the plan owner via `PlanCtx::ensure_budget`) so that a plan fanning
        // out to up to 13 A* calls is bounded by ONE `node_cap`, not `node_cap` per call. A ctx with no
        // shared counter (only a raw unit test constructs one) falls back to a local count, which for a
        // single call is the same thing.
        let mut local_expanded = 0usize;
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
            // ZONE-LINE ARRIVAL (#229): a zone line is a VOLUME (a DRNTP BSP region), not a point.
            // Its representative point's z is an interior point of that volume — structurally never a
            // floor height — so "did I reach the goal cell at the goal's tier?" is the wrong question
            // to ask of it. Ask the right one instead: am I STANDING INSIDE the region? That is
            // exactly the predicate the native auto-cross fires on. Only tested near the goal cell,
            // so it costs a handful of BSP walks per plan, not one per node.
            if let (Some(want), Some(water)) = (ctx.goal_region, self.water.as_ref()) {
                if h(c, r) <= 2.0 * cell {
                    let p = center(c, r);
                    if water.zone_line_at(p[0], p[1], fz + 1.0) == Some(want) {
                        goal_key = Some(ckey);
                        break;
                    }
                }
            }
            if (c, r) == (gc, gr) {
                if (fz - goal_floor).abs() <= GOAL_TIER_TOL {
                    goal_key = Some(ckey); // reached the goal cell on the requested tier — done
                    break;
                }
                // Wrong tier: remember the first (cheapest) one, but keep searching — the right tier
                // may be reachable by climbing to it at an adjacent cell. Fall through and expand.
                if goal_fallback.is_none() { goal_fallback = Some(ckey); }
            }
            // The ONE runaway bound, and it is a deterministic, PLAN-WIDE node count (#394 + review).
            // Increment the shared running total (or a local one for a bare unit-test ctx) and stop
            // when it passes `node_cap`. Because every A* call in the plan shares this counter, a plan
            // that fans out to 13 calls still costs at most `node_cap` expansions in total, not per
            // call — which is what makes the "one plan, one budget" contract (#340) true rather than
            // merely documented. Whichever cap bites, the same query hits it after the same number of
            // expansions on every machine — `Exhausted(NodeCap)` reproducibly, instead of `SearchClosed`
            // on a fast box and `Exhausted(Deadline)` on a slow one. The wall-clock check that used to
            // sit here is deleted; there is no `Instant::now()` in the search at all now.
            let total = match &ctx.expanded {
                Some(shared) => shared.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1,
                None => { local_expanded += 1; local_expanded }
            };
            if total > node_cap { limit = Some(PlanLimit::NodeCap); break; }
            // Track the closest-to-goal cell reached (heuristic = straight-line cells to the goal),
            // for the partial-path fallback below.
            let hd = h(c, r);
            if hd < best_toward_h { best_toward_h = hd; best_toward = Some(ckey); }
            let cz = fz;
            let g_cur = *g_score.get(&ckey).unwrap_or(&f32::MAX);
            // Expand from the CHARACTER'S OWN position for the start node (see `anchor_at_char`), so
            // every first-hop edge is clearance-tested from where the walker actually stands rather
            // than from a cell centre it may not be able to reach. Every other node expands from its
            // cell centre as before.
            let a = if anchor_at_char && ckey == skey { [start[0], start[1]] } else { center(c, r) };
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
                    if !self.edge_clear([a[0], a[1], cz + CHEST], [b[0], b[1], nf + CHEST], radius, cell) { continue; }
                    if !self.edge_clear([a[0], a[1], cz + FEET_CLR], [b[0], b[1], nf + FEET_CLR], radius, cell) { continue; }
                    // LEDGE margin. The rays above only see geometry that is IN THE WAY; this is the
                    // one test that sees geometry that is MISSING (a drop, a bridge edge, a
                    // waterline). Only the GENEROUS tier asks for it: at the minimum tier the promise
                    // is exactly "the character fits", which is what keeps a narrow bridge or a
                    // gangplank routable at all (see `search_tiered`). Memoised per (cell, floor)
                    // — a cell is reached from up to 8 neighbours and the probe is 4 column queries.
                    if ledge_margin > 0.0 && !*margin_ok.entry(nkey).or_insert_with(||
                        self.ground_margin_ok(b[0], b[1], nf, ledge_margin)) { continue; }
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
                            if !self.edge_clear([a[0], a[1], cz + CHEST], [land[0], land[1], nf + CHEST], radius, cell) {
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
                    // Submerged (water ABOVE us), or floating AT the surface — a start anchored to
                    // the water surface (#329/#197p2) has air above it, so the old submerged-only
                    // test never fired for it and a floating swimmer had no way OUT of the water.
                    let submerged = (0..=3).any(|k| water.is_water(a[0], a[1], cz + 2.0 + k as f32 * 4.0))
                        || water.is_water(a[0], a[1], cz - 1.0);
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
                            if !self.edge_clear([a[0], a[1], ray_z + CHEST], [b[0], b[1], nf + CHEST], radius, cell) { continue; }
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
                if self.edge_clear([a[0], a[1], cz + CHEST], [b[0], b[1], cz + CHEST], radius, cell) {
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
        // The ANCHORS the whole search hung off. Log them on the SUCCESS path too, not just on
        // failure: every anchor bug in this file's history (#229 goal-on-a-phantom-tier, #329
        // start-on-the-ceiling, #197p2 start-on-the-pool-bottom) presented as "A* returns a route
        // the walker can't follow" — a success with visibly wrong anchors. Without this line each
        // one cost days; with it, each is a 30-second read. Coarse whole-zone plans only (the fine
        // local tier re-plans every tick and would spam).
        if max_search.is_none() {
            tracing::info!("find_path: start=({:.0},{:.0},{:.1}) start_floor={:.2}{} goal=({:.0},{:.0},{:.1}) goal_floor={:.2}{}",
                start[0], start[1], start[2], start_floor,
                if floating_surface.is_some() { " (WATER SURFACE — floating)" } else { "" },
                goal[0], goal[1], goal[2], goal_floor,
                match ctx.goal_region { Some(i) => format!(" (zone-line region {i})"), None => String::new() });
        }
        // How much straight-line ground toward the goal the best-reached cell actually closes. The
        // caller uses this to decide whether a partial route is a STAGE of a journey (worth walking,
        // then re-planning) or a SHUFFLE into a wall (#337) — see `PARTIAL_MIN_UNITS`.
        let progress = (h(sc, sr) - best_toward_h).max(0.0);
        // Prefer the requested tier; fall back to a wrong-tier goal only if the right tier is
        // unreachable (keeps the old "reach the goal cell at all" behaviour as a floor).
        let (goal_key, reached_goal) = match goal_key.or(goal_fallback) {
            Some(k) => (k, true),
            None => {
                // Partial route toward the frontier (#188). Built whenever the search made ANY
                // progress; whether it may be WALKED is the caller's call, and hinges on the one
                // question that used to be unanswerable: was the frontier closed, or did we just run
                // out of clock? `find_path_ex` walks it only under `Exhausted` and only past
                // `PARTIAL_MIN_UNITS`; a CLOSED search now reports `Unreachable` and no waypoints
                // at all, instead of handing the walker a greedy stub to wedge on (#337).
                let progressed = best_toward.is_some() && best_toward_h + 1.0 < h(sc, sr);
                match best_toward {
                    Some(bk) if progressed => {
                        tracing::info!("find_path: partial route toward goal (this-call expansions={}, {:.0}->{:.0} UNITS from goal, {})",
                            closed.len(), h(sc, sr), best_toward_h,
                            match limit { Some(l) => l.as_str(), None => "frontier CLOSED — goal is UNREACHABLE" });
                        (bk, false)
                    }
                    _ => {
                        match limit {
                            Some(l) => tracing::warn!("find_path: search hit a limit ({}) after {} nodes with no usable \
                                route — start_floor={start_floor} goal_floor={goal_floor}. This is NOT 'no route': the \
                                frontier was never closed.", l.as_str(), closed.len()),
                            None => tracing::info!("find_path: NO ROUTE — frontier closed after {} nodes \
                                (start_floor={start_floor}, goal_floor={goal_floor})", closed.len()),
                        }
                        return Search { limit, progress, closed_n: closed.len(), ..Default::default() };
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
        // Edge margin (#312): A* routes through 8u cell CENTERS, so a cell that merely touches a
        // wall/ledge/waterline still counts as walkable and the followed straight line hugs that
        // boundary — the walker then clips the edge and falls off or wedges (the #314 city-wall
        // corner is exactly this). Inset each waypoint away from any unwalkable side by ~the
        // collision radius: sample the floor a margin out in ±E/±N; a side with no floor in a tight
        // band around the waypoint's z is a wall/drop/water edge, so nudge away from it. Opposing
        // walls (a narrow corridor) cancel out — the centre line is kept. A corner pushes away from
        // BOTH sides, unwedging it.
        //
        // NOTE (#358): this only sees sides where the FLOOR RUNS OUT — ledges, drops, waterlines. A
        // wall standing on continuous floor is invisible to it (there is still floor a margin out,
        // at the wall's foot), so the inset is not, and never was, what keeps a route off a WALL.
        // Measured on the live gfaydark→butcher wedge: the inset moved not one waypoint, and the
        // routes before and after widening the margin were byte-identical. Clearance from walls is
        // enforced by the edge test (`edge_clear`), not here.
        let margin = radius.max(1.0).min(cell * 0.45);
        // Walkable a margin out = a floor exists within a tight vertical band of the waypoint (so a
        // wall lip above, a drop below, or water all read as "edge" and get avoided).
        let edge_ok = |x: f32, y: f32, z: f32| -> bool {
            self.nearest_floor(x, y, z, 3.0, 8.0).map_or(false, |f| (f - z).abs() <= 8.0)
        };
        for i in 0..path.len() {
            let [x, y, z] = path[i];
            let mut push = [0.0f32, 0.0f32];
            if !edge_ok(x + margin, y, z) { push[0] -= 1.0; }
            if !edge_ok(x - margin, y, z) { push[0] += 1.0; }
            if !edge_ok(x, y + margin, z) { push[1] -= 1.0; }
            if !edge_ok(x, y - margin, z) { push[1] += 1.0; }
            let len = (push[0] * push[0] + push[1] * push[1]).sqrt();
            if len > 0.0 {
                let (nx, ny) = (x + push[0] / len * margin, y + push[1] / len * margin);
                // Two guards, because the inset faces two hazards and `edge_ok` only sees one.
                //
                // `edge_ok` asks "is there still FLOOR there" — it is what stops the nudge shoving a
                // waypoint off the mesh. It is structurally incapable of seeing a WALL: `column_hits`
                // discards every triangle with `tri_nz <= 0` before intersecting, so a vertical face
                // can never be a `nearest_floor` hit. There is floor at a wall's foot, so `edge_ok`
                // happily green-lights a nudge straight INTO one (#358).
                //
                // That was survivable while the inset was small, but the tiered planner (see
                // `search_tiered`) now plans at a GENEROUS `radius`, which makes `margin` — and so
                // the push — correspondingly bigger. A bigger push validated by a wall-blind guard is
                // how you end up walking closer to walls than before. So also require the character's
                // own collision volume to actually FIT where we are moving it: `footprint_clear` is
                // the same ring test the controller's depenetration net uses.
                if edge_ok(nx, ny, z) && self.footprint_clear(nx, ny, z, radius, 8) {
                    path[i] = [nx, ny, z];
                }
            }
        }
        // Snap the final waypoint to the exact goal only when we actually reached the goal cell; a
        // partial path must end at the reachable cell, not clip toward an unreachable goal.
        if reached_goal {
            if let Some(last) = path.last_mut() { *last = [goal[0], goal[1], goal[2]]; }
        } else {
            // A PARTIAL route can end inside a submerged dead-end pocket (e.g. a sunken pool whose
            // walls exceed the climb-out grade): `best_toward` picks the cell with the best
            // straight-line heuristic to the goal, with no way to know that continuing from a water
            // cell dead-ends (the water-ascent haul-out cap makes it topologically a trap). Walking
            // the char IN gets it stuck oscillating until it stalls out — see #259. Trim trailing
            // submerged waypoints so a partial route stops at the dry edge instead of driving into
            // the trap; an honest "no route" beats a one-way walk into a pit.
            while path.last().is_some_and(|&wp| self.in_water(wp)) {
                path.pop();
            }
            if path.is_empty() {
                return Search { limit, progress, closed_n: closed.len(), ..Default::default() };
            }
        }
        // THE ROUTE BEGINS AT THE CHARACTER (#229's last mile, part 2). The walker follows the route
        // with pure pursuit, which steers along the segment (path[i], path[i+1]) — it ASSUMES path[0]
        // is where the character is standing. A* returns a start-EXCLUSIVE route, so the walker never
        // actually aims at the first waypoint: it projects itself onto the path[0]→path[1] segment
        // and steers along THAT, cutting the corner between them.
        //
        // When the first leg is a real manoeuvre, that corner-cut is fatal. Live everfrost: the
        // character is pressed against a wall, A* correctly routes NORTH off the wall and then west
        // around it — but pure pursuit projected the character onto the (north-waypoint → west-
        // waypoint) segment, aimed south-west along it, and drove straight back into the wall. It
        // made no progress, re-planned the identical (correct) route, and stalled out after 8
        // attempts. Prepending the character's own position makes the first pursuit segment
        // character→first-waypoint, so the route's first leg is actually walked.
        path.insert(0, [start[0], start[1], start_floor]);
        Search {
            path: Some((path, reached_goal)),
            limit,
            no_route: None,
            progress: if reached_goal { 0.0 } else { progress },
            closed_n: closed.len(),
        }
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

    /// #257: the net-thread wall-clock budget (PLAN_BUDGET_MS) must be generous enough that a
    /// large but ordinary open-terrain plan still routes ALL the way to the goal — the cap only
    /// truncates pathological searches, never a legitimate long walk. A big empty floor is the
    /// cheap-per-node case, so a full corner-to-corner route completes far under the budget.
    #[test]
    fn find_path_large_open_plan_is_not_truncated_by_time_cap() {
        let big = MeshData {
            positions: vec![
                [-320.0, 0.0, -320.0], [320.0, 0.0, -320.0], [320.0, 0.0, 320.0], [-320.0, 0.0, 320.0],
            ],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![big], objects: vec![], textures: vec![] }, 32.0);
        // World [east, north, up]: opposite corners of the plane, ~800u apart (~100 nav cells).
        let path = col.find_path([-300.0, -300.0, 0.0], [300.0, 300.0, 0.0], 1.0, &[], false)
            .expect("a large open plane must route fully corner-to-corner within the time budget");
        let last = *path.last().unwrap();
        assert!((last[0] - 300.0).abs() < 8.0 && (last[1] - 300.0).abs() < 8.0,
            "route must reach the far corner (not a truncated partial), got {last:?}");
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

    // ── nav z-anchor regression tests (#229, #329, #197p2) ───────────────────────────────────────
    // A quad in EQ WLD space (pos = [north, height, east]). `up` picks the winding, and therefore
    // the face normal: an up-facing FLOOR you can stand on, or a down-facing CEILING you cannot.
    fn slab(z: f32, n0: f32, n1: f32, e0: f32, e1: f32, up: bool) -> MeshData {
        MeshData {
            positions: vec![[n0, z, e0], [n0, z, e1], [n1, z, e1], [n1, z, e0]],
            normals: vec![], uvs: vec![],
            indices: if up { vec![0, 1, 2, 0, 2, 3] } else { vec![0, 2, 1, 0, 3, 2] },
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    // ─── RETRACTED at D-2 (#375): three old #329 guards whose premise qcat falsified ───
    //
    // `nearest_floor_never_returns_a_ceiling`, `fallback_never_admits_a_ceiling_whose_floor_is_below_
    // the_query_window`, and `fallback_admits_the_inverted_ground_but_not_the_ceiling_above_it` all
    // asserted that a DOWN-facing surface with OPEN space above it (a lone ceiling at -55.97 over a
    // floor 14u below; a roof at 391.8 over a floor 462u below) is a ceiling `nearest_floor` must never
    // return. The D-2 shape probe (`probe_qcat_column_vs_fixture`) MEASURED the character's walkable
    // qcat surface at -42.97 to be EXACTLY that shape — down-facing, open above, floor ~13u below — so
    // "down-facing + open above = ceiling" is false: it is walkable floor (the #375 fix). A facing-blind
    // classifier that rejected those synthetic ceilings would also delete qcat's walkway.
    //
    // The genuine #329 protection is preserved and re-tested by:
    //   * `close_roof_ceiling_is_rejected_by_headroom` — a ceiling with a roof CLOSE above (headroom <
    //     NAV_AGENT_HEIGHT) is rejected. That is what makes a ceiling a ceiling — a roof — not its
    //     winding. Mutation-checked.
    //   * `qcat_pocket_nearest_floor_is_never_the_ceiling` — the FAR qcat roof (391.8) is never returned
    //     at a REALISTIC ref_z (the -66 floor), because the `ref_z ± window` excludes it. (The old
    //     `fallback_never_admits…` queried AT roof height, ref_z=391.8 — a position a character is never
    //     in; the window defence is the real one.)
    // `the_fallback_reports_itself_so_nav_degraded_is_never_silent` is removed with the `column_bottom`
    // valve it tested; D-3 replaces the honesty signal (`nav_degraded/inverted_floor_art` →
    // `nav_support/facing_blind_ground`).
    //
    // The two tests below that assert inverted GROUND is still found (`column_whose_only_surface_is_
    // inverted…`, `a_fully_inverted_zone…`) STILL PASS under D-2 (an inverted floor with clearance above
    // is standable) and are kept.

    /// Inverted GROUND: a column whose lowest (and here only) surface is down-facing, with nothing
    /// beneath it. Real zones bake standable ground this way — permafrost loses 131 such columns to
    /// the facing filter and neriakc 14 — and deleting it leaves the planner with no floor in a
    /// column that plainly has ground in it. It is ground precisely because nothing lies under it; a
    /// ceiling always has a floor beneath. So the filter must never delete the ONLY ground in a
    /// column, and the rule is scoped to the COLUMN rather than to a whole-zone winding verdict
    /// (which votes on the bottom of each column and so cannot see inversion higher up).
    ///
    /// NOT the highpass shape, despite the resemblance: highpass's inverted band all has correctly
    /// wound ground BENEATH it, so it is a ceiling by this rule and stays deleted. This fallback does
    /// not fire there and does not recover highpass's loss — see the LIMIT note on `column_hits`.
    #[test]
    fn column_whose_only_surface_is_inverted_still_finds_a_floor() {
        let assets = ZoneAssets {
            terrain: vec![
                slab(0.0, 0.0, 256.0, 0.0, 256.0, true),      // large, correctly-wound floor
                slab(50.0, 0.0, 8.0, 300.0, 308.0, false),    // isolated inverted GROUND — nothing under it
            ],
            objects: vec![], textures: vec![],
        };
        let col = Collision::build(&assets, 8.0);

        // Query off the quad's diagonal split (300,0)-(308,8) so the ray crosses exactly one of its
        // two triangles, not the shared edge.
        // Facing-blind: the surface is there, and it is down-facing — mis-wound ground.
        let blind = col.column_surfaces(306.0, 2.0, 50.0, 20.0, 100.0);
        assert_eq!(blind.len(), 1, "exactly one surface lives in this column");
        assert!(blind[0].1 < 0.0, "and it is down-facing — the only ground here is inverted");

        // The filtered query must still find it: this column has nothing beneath it to make it a
        // ceiling, so deleting it leaves NO floor at all where there plainly is ground.
        let f = col.nearest_floor(306.0, 2.0, 50.0, 20.0, 100.0);
        assert!(f.is_some(), "the filter must never delete the only ground in a column");
        assert!((f.unwrap() - 50.0).abs() < 0.1, "expected the inverted surface's own height, got {f:?}");
        assert_eq!(col.column_floors(306.0, 2.0, 50.0, 20.0, 100.0).len(), 1);
    }

    /// A whole zone can be the degenerate case of `column_whose_only_surface_is_inverted_still_finds_a_floor`
    /// above: EVERY column's only surface is inverted (a mesh whose winding convention is entirely
    /// backwards), not just one isolated patch. There is no whole-zone gate anymore to catch or
    /// report this — the per-column fallback in `column_hits` handles it uniformly, one column at a
    /// time, with no distinction between "one bad column" and "every column is bad". The zone must
    /// stay navigable either way.
    #[test]
    fn a_fully_inverted_zone_keeps_every_floor_via_the_per_column_fallback() {
        // Every face inverted (down-facing) — a mesh whose winding convention is backwards.
        let assets = ZoneAssets {
            terrain: vec![slab(0.0, 0.0, 96.0, 0.0, 96.0, false)],
            objects: vec![], textures: vec![],
        };
        let col = Collision::build(&assets, 8.0);
        // The filter would delete this single, down-facing surface — but that's the only ground in
        // every column here, so the fallback keeps it (fail-old), rather than deleting it and
        // leaving the zone unnavigable (fail-empty).
        assert!(col.nearest_floor(20.0, 40.0, 1.0, 20.0, 100.0).is_some(),
            "an inverted mesh must keep its floors via the per-column fallback");
        assert!(col.find_path([8.0, 8.0, 0.0], [56.0, 56.0, 0.0], 1.0, &[], false).is_some(),
            "and must still route");
    }

    /// #229: a `zone_cross` aims at a DRNTP region's interior point, whose z is a point in a VOLUME
    /// — on the shipped zones 1.5u to 127u ABOVE the real floor, and never a floor height. Such a
    /// goal must be projected DOWN onto the floor beneath it; snapping it to the "nearest surface
    /// within ±20" found nothing to grab (or a phantom), and the goal tier was then unreachable.
    #[test]
    fn goal_high_in_a_volume_projects_onto_the_floor_beneath_it() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 64.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 8.0);
        // 47u up — the gfaydark→butcher region z. The old ±STEP_UP(20) window can't even see the floor.
        assert_eq!(col.nearest_floor(32.0, 32.0, 47.28, 20.0, 20.0), None,
            "the old ±20 snap window has no surface to grab at a volume point");
        let f = col.floor_beneath(32.0, 32.0, 47.28, 2.0, 400.0).expect("the floor is beneath it");
        assert!(f.abs() < 0.01, "the goal must project onto the floor at 0, got {f}");
        // And a route to that airborne goal now completes (A* accepts arrival on the real floor).
        assert!(col.find_path([8.0, 8.0, 0.0], [56.0, 56.0, 47.28], 1.0, &[], false).is_some(),
            "a goal 47u above the floor must still route to the floor under it");
    }

    /// #329 / #197p2: a character FLOATING in water is supported by the WATER SURFACE, not by a
    /// slab. Anchoring it to the nearest surface sent A* along the pool BOTTOM (halas: 128u down),
    /// so the walker dived to the planned floor and stranded. The route must stay at the surface.
    #[test]
    fn floating_swimmer_is_anchored_to_the_water_surface_not_the_pool_bottom() {
        let assets = ZoneAssets {
            terrain: vec![slab(-100.0, 0.0, 64.0, 0.0, 48.0, true),  // deep pool bottom
                          slab(0.0, 0.0, 64.0, 48.0, 96.0, true)],   // dry bank, at the waterline
            objects: vec![], textures: vec![],
        };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(0.0))));

        // Floating 2u under the surface, 98u above the only solid floor.
        let start = [8.0, 32.0, -2.0];
        let goal  = [72.0, 32.0, 0.0]; // the bank
        let path = col.find_path(start, goal, 1.0, &[], false).expect("a swimmer must reach the bank");
        let deepest = path.iter().fold(f32::MAX, |m, w| m.min(w[2]));
        assert!(deepest > -10.0,
            "the route must cross AT THE SURFACE, not dive to the -100 pool bottom (deepest wp z = {deepest})");
        let last = path.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 8.0 && (last[1] - goal[1]).abs() < 8.0, "and it must reach the bank");
    }

    /// A character WADING (feet on the bottom, water shallow) is still anchored to the ground — the
    /// water-surface anchor is only for a character with no footing under it.
    #[test]
    fn wading_character_is_still_anchored_to_the_ground() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 64.0, true)], objects: vec![], textures: vec![] };
        let mut col = Collision::build(&assets, 8.0);
        col.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::flat_below(3.0))));
        // Standing on the floor in 3u of water: there IS footing, so no surface anchor.
        let path = col.find_path([8.0, 32.0, 0.0], [56.0, 32.0, 0.0], 1.0, &[], false)
            .expect("a wader must still route across the shallows");
        assert!(path.iter().all(|w| w[2] < 1.0), "waypoints stay on the ground, not up on the waterline");
    }

    /// #229's last mile: A* expands each node from its CELL CENTRE, but the walker drives from where
    /// the character actually STANDS. Anchoring the start at the cell centre let A* emit a route
    /// whose every cell-centre segment was clear while the character→waypoint[0] leg ran straight
    /// through a wall — the walker pressed into it, made no progress, re-planned the identical route
    /// and stalled out. (Live everfrost→blackburrow: exactly 1 of 99 segments blocked, the first.)
    /// EVERY segment of a returned route, INCLUDING the first one from the character's real
    /// position, must be clearance-clear.
    #[test]
    fn route_first_leg_is_walkable_from_the_characters_real_position() {
        // A wall running north–south at east=44 (north 0..52), with the way around it to the north.
        let wall = MeshData {
            positions: vec![[0.0, 0.0, 44.0], [52.0, 0.0, 44.0], [52.0, 12.0, 44.0], [0.0, 12.0, 44.0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let assets = ZoneAssets {
            terrain: vec![slab(0.0, 0.0, 96.0, 0.0, 96.0, true), wall],
            objects: vec![], textures: vec![],
        };
        let col = Collision::build(&assets, 8.0);

        // The character stands just WEST of the wall, off-centre in its nav cell; the goal is EAST of
        // the wall and south, so any route must first go north around the wall's end.
        let start = [40.0, 50.0, 0.0];
        let goal  = [60.0, 10.0, 0.0];
        let path = col.find_path(start, goal, 1.0, &[], false).expect("a route around the wall exists");

        let mut prev = start;
        for (i, w) in path.iter().enumerate() {
            assert!(col.path_clear([prev[0], prev[1], prev[2] + 3.0], [w[0], w[1], w[2] + 3.0], 1.0),
                "segment {i} ({prev:?} -> {w:?}) crosses the wall — the walker cannot follow it");
            prev = *w;
        }
    }

    /// #340/#394: the search must honour the CALLER's NODE CAP instead of re-arming a fresh one per
    /// call. `plan_path` makes up to 13 A* calls per plan; a per-call budget let one plan cost 13× its
    /// intended bound. `PlanCtx::default()` must arm NO tight cap (only the global `MAX_NODES`
    /// backstop): a bare call runs to completion.
    #[test]
    fn find_path_honours_a_caller_supplied_node_cap() {
        // A long open corridor — the goal is far enough that A* must expand well over a handful of
        // nodes, so a tiny node cap must actually cut the search short before it can reach the goal.
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 1600.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 32.0);
        let (start, goal) = ([8.0, 32.0, 0.0], [1560.0, 32.0, 0.0]);

        let fresh = col.find_path_res(start, goal, 1.0, &[], false, 8.0, None, 0.0, PlanCtx::worker());
        assert!(fresh.is_some(), "with the worker's (generous) cap the route is found");
        assert!(col.find_path_res(start, goal, 1.0, &[], false, 8.0, None, 0.0, PlanCtx::default()).is_some(),
            "and with the default (MAX_NODES backstop) it is found too — a bare search runs to completion");

        // A cap of 4 nodes cannot reach a goal ~190 cells away. Note the outcome is DETERMINISTIC:
        // this asserts identically on a fast box and a slow one, which the old wall-clock version
        // could not (that was the #394 bug).
        let tiny = PlanCtx { node_cap: Some(4), ..PlanCtx::default() };
        let out = col.find_path_res(start, goal, 1.0, &[], false, 8.0, None, 0.0, tiny);
        assert!(out.is_none(), "a 4-node cap must abort the search before it reaches a far goal");
    }

    /// **#337/#356/#394 — the honesty invariant, made DETERMINISTIC.** A search that hit its NODE CAP
    /// must report `Exhausted` ("I don't know"), and a search that CLOSED its frontier without finding
    /// the goal must report `Unreachable` ("no"). Collapsing those two is what made the walker drive a
    /// stub into a wall and freeze at `blocked` for months.
    ///
    /// Same geometry, same goal, same code path: only the node cap differs. The answers must differ
    /// too — and the cut-short search must NEVER be the one that says "no route". Unlike the wall-clock
    /// version this replaced (#394), the answer here does not depend on machine speed: a 4-node cap is
    /// hit after exactly 4 expansions on every machine.
    #[test]
    fn a_node_cap_is_never_reported_as_no_route() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 1600.0, true)], objects: vec![], textures: vec![] };
        let col = Collision::build(&assets, 32.0);
        let start = [8.0, 32.0, 0.0];

        // (a) Reachable goal, generous cap → a complete Route.
        let reachable = [1560.0, 32.0, 0.0];
        assert!(matches!(col.find_path_ex(start, reachable, 1.0, &[], 8.0, None, 0.0, PlanCtx::default()),
            PlanOutcome::Route(_)), "a reachable goal with a generous cap must produce a complete Route");

        // (b) The SAME reachable goal, with a cap too small to reach it → Exhausted(NodeCap), NEVER
        //     Unreachable. The search stopped LOOKING; it did not prove there is no route.
        let tiny = PlanCtx { node_cap: Some(4), ..PlanCtx::default() };
        match col.find_path_ex(start, reachable, 1.0, &[], 8.0, None, 0.0, tiny) {
            PlanOutcome::Exhausted { limit: PlanLimit::NodeCap, .. } => {}
            other => panic!("a search cut short by its node cap must report Exhausted(NodeCap) — reporting \
                             {other:?} for a goal that IS reachable is the #337 lie"),
        }

        // (c) A goal OFF the mesh entirely, generous cap → a definitive Unreachable, and no waypoints.
        let off_mesh = [1560.0, 3000.0, 0.0]; // far outside the slab: no walkable floor at all
        let out = col.find_path_ex(start, off_mesh, 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        match &out {
            PlanOutcome::Unreachable(NoRoute::GoalNotWalkable) => {}
            other => panic!("a goal with no walkable floor must fail IMMEDIATELY as Unreachable(GoalNotWalkable), got {other:?}"),
        }
        assert!(out.route().is_none(), "an unreachable goal must hand back NO route");
    }

    /// **A SLOPPY GOAL Z IS NOT AN UNREACHABLE GOAL.** Agents routinely pass a rough z (0, or a map
    /// coordinate) for a goal whose real floor sits well above it. Rejecting those as
    /// `goal_not_walkable` is a FALSE definitive no — the XY is perfectly walkable, and `main`
    /// routed to it fine. Caught live in North Qeynos: `goto (-40,250,z=0)` refused to move at all.
    ///
    /// The honest line is "is there ANY floor at this XY?": a bad z snaps to the real floor; a goal
    /// off the mesh entirely still fails hard (asserted above).
    #[test]
    fn a_goal_with_a_sloppy_z_still_routes_to_its_real_floor() {
        // Floor at z = 40. The caller asks for z = 0 — 40u BELOW it, far outside any tier tolerance,
        // and `floor_beneath` only ever looks DOWN, so the old code resolved nothing and hard-failed.
        let col = Collision::build(
            &ZoneAssets { terrain: vec![slab(40.0, 0.0, 200.0, 0.0, 200.0, true)], objects: vec![], textures: vec![] },
            32.0);
        let out = col.find_path_ex([16.0, 16.0, 40.0], [180.0, 180.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        let route = match &out {
            PlanOutcome::Route(p) => p,
            other => panic!("a walkable XY with a sloppy z must still ROUTE (snapping to its real \
                             floor), not be dismissed as unreachable — got {other:?}"),
        };
        let last = *route.last().unwrap();
        assert!((last[0] - 180.0).abs() < 8.0 && (last[1] - 180.0).abs() < 8.0,
            "and the route must reach the goal XY, got {last:?}");
    }

    /// **A BOXED-IN START MUST NEVER BE REPORTED AS "NO ROUTE TO THE GOAL".**
    ///
    /// The two failures look identical from outside — the frontier closes, the goal isn't in it —
    /// and conflating them produces a *false definitive no*, which is worse than the silent wedge
    /// this PR set out to kill: the agent is told, with confidence, something untrue.
    ///
    /// Caught LIVE, not by a test: in gfaydark the walker wedged on terrain, A* closed after ONE
    /// node, and the cell-centre retry dribbled out a 2-cell partial — which an earlier version of
    /// `search()` mistook for evidence that the zone had really been surveyed. It reported
    /// `no_path: search_closed` for a goal that was perfectly reachable from 16u away.
    #[test]
    fn a_boxed_in_start_is_start_isolated_not_no_route() {
        // A big open plane the goal sits on, plus a tiny sealed box around the START only.
        let wall = |n0: f32, e0: f32, n1: f32, e1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e1], [n1, 40.0, e1], [n0, 40.0, e0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // The pocket is a FEW cells across, not one — which is what makes this bite. The character
        // can shuffle a couple of cells inside it, so the search dribbles out a small partial route
        // "toward" the goal. That stub is exactly what fooled the earlier version into believing the
        // search had surveyed the zone. (A single-cell box produces no partial at all and would let
        // the bug through.)
        let (n0, n1, e0, e1) = (88.0f32, 120.0f32, 88.0f32, 120.0f32); // ~4 nav cells across
        let terrain = vec![
            slab(0.0, 0.0, 400.0, 0.0, 400.0, true),
            wall(n0, e0, n0, e1),
            wall(n1, e0, n1, e1),
            wall(n0, e0, n1, e0),
            wall(n0, e1, n1, e1),
        ];
        let col = Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);

        // Start in the pocket's FAR corner, so shuffling across it gains real ground on the goal —
        // the search will produce a partial. The goal is wide open and obviously walkable: it is the
        // START that is sealed in.
        let out = col.find_path_ex([92.0, 92.0, 0.0], [350.0, 350.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        match &out {
            PlanOutcome::Unreachable(NoRoute::StartIsolated) => {}
            PlanOutcome::Unreachable(NoRoute::SearchClosed) => panic!(
                "a boxed-in START reported as `search_closed` — that is a FALSE definitive 'no route to \
                 the goal' for a goal that is perfectly reachable. The character is stuck, not the goal."),
            other => panic!("expected Unreachable(StartIsolated), got {other:?}"),
        }
        assert!(out.route().is_none(), "and no stub route out of the sealed cell — the walker must not drive it");
    }

    /// **The FINE LOCAL STEERING tier must keep its partial route even from a boxed-in start.**
    ///
    /// This is the test that was missing for live-bug #2 (the halas swimmer). `find_path_res` is the
    /// fine 2u tier the walker steers on; its searches are bounded to 40u, so their explored
    /// component is *always* small and they look "boxed in" by construction — a floating swimmer at
    /// a shoreline especially so. An earlier `search()` wiped the partial on that path, the swimmer
    /// lost its steering hint, stopped swimming, and wedged at the water's edge for 8 attempts while
    /// the coarse planner cheerfully re-issued a perfect 78-waypoint route across the water.
    ///
    /// The honest planner API (`find_path_ex`) must still say `start_isolated` and hand back NO
    /// route — an "Unreachable" carries no waypoints. Both halves are asserted here: they are
    /// different questions, and this is exactly where they diverge.
    #[test]
    fn a_boxed_in_start_still_yields_a_partial_for_local_steering() {
        let wall = |n0: f32, e0: f32, n1: f32, e1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e1], [n1, 40.0, e1], [n0, 40.0, e0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let (n0, n1, e0, e1) = (88.0f32, 120.0f32, 88.0f32, 120.0f32);
        let terrain = vec![
            slab(0.0, 0.0, 400.0, 0.0, 400.0, true),
            wall(n0, e0, n0, e1), wall(n1, e0, n1, e1),
            wall(n0, e0, n1, e0), wall(n0, e1, n1, e1),
        ];
        let col = Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);
        let (start, goal) = ([92.0, 92.0, 0.0], [350.0, 350.0, 0.0]);

        // The steering tier: it asked for a best-effort route and it must GET one. Starving it here
        // is what stopped the swimmer.
        let steer = col.find_path_res(start, goal, 1.0, &[], true, 8.0, None, 0.0, PlanCtx::default());
        assert!(steer.is_some(),
            "the FINE LOCAL STEERING tier (allow_partial) must still get a partial route from a boxed-in \
             start — wiping it is what stopped the halas swimmer dead at the water's edge");
        assert!(steer.unwrap().len() >= 2, "and it must be something the walker can actually steer along");

        // The honest planner API, on the very same search, must still refuse to call that a route.
        let out = col.find_path_ex(start, goal, 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        assert!(matches!(out, PlanOutcome::Unreachable(NoRoute::StartIsolated)),
            "the honest API must still report start_isolated, got {out:?}");
        assert!(out.route().is_none(), "an Unreachable must carry no waypoints");
    }

    /// A partial route may only be walked when it makes GENUINE progress toward the goal. The old
    /// bar was one nav cell (8u) — enough for a wedged character to shuffle a single cell into a
    /// wall and call it a plan (#337). Under a frontier-CLOSED search there is no partial at all.
    #[test]
    fn a_closed_search_yields_no_partial_route() {
        // A slab with a sealed pocket at the far corner: the goal has a perfectly good floor (so it
        // is NOT dismissed up front), but two walls seal it off — so the search has to close the
        // whole slab to learn there is no way in.
        // A vertical wall (n0,e0)->(n1,e1), 30u tall.
        let wall = |n0: f32, e0: f32, n1: f32, e1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e1], [n1, 30.0, e1], [n0, 30.0, e0]],
            normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let terrain = vec![
            slab(0.0, 0.0, 200.0, 0.0, 200.0, true),
            wall(160.0, 160.0, 160.0, 200.0), // along east, at north=160
            wall(160.0, 160.0, 200.0, 160.0), // along north, at east=160
        ];
        let col = Collision::build(&ZoneAssets { terrain, objects: vec![], textures: vec![] }, 32.0);

        let out = col.find_path_ex([16.0, 16.0, 0.0], [180.0, 180.0, 0.0], 1.0, &[], 8.0, None, 0.0, PlanCtx::default());
        assert!(matches!(out, PlanOutcome::Unreachable(_)),
            "a sealed goal, searched to completion, is UNREACHABLE — got {out:?}");
        assert!(out.route().is_none(), "and it must hand back no waypoints at all (the #337 lie is a partial here)");
        // The old code walked this: `find_path(.., allow_partial=true)` would have returned a greedy
        // stub toward the pocket. It is still available for LOCAL STEERING, which is the only place
        // a partial belongs — but the honest planner API above refuses to call it a route.
    }

    /// #229: `find_zone_line_near` must hand back a point a character can STAND on — the region's
    /// own z is an interior point of the trigger volume, and walking to it was never possible.
    /// The projected point must still be inside the region, so the auto-cross still fires there.
    #[test]
    fn zone_line_target_is_projected_onto_the_floor_and_stays_inside_the_region() {
        let assets = ZoneAssets { terrain: vec![slab(0.0, 0.0, 64.0, 0.0, 64.0, true)], objects: vec![], textures: vec![] };
        let mut col = Collision::build(&assets, 8.0);
        // A zone-line region filling everything below z=50 — its representative point is up in the
        // volume, tens of units above the floor at 0 (exactly the shipped-asset shape).
        col.set_water(Some(std::sync::Arc::new(crate::region_map::RegionMap::zone_line_below(50.0, 7))));

        let (idx, p) = col.find_zone_line_near(Some(7), [8.0, 8.0, 0.0]).expect("the zone line is found");
        assert_eq!(idx, 7);
        assert!(p[2].abs() < 0.01, "the target must sit on the floor (z=0), not up in the volume: got {}", p[2]);
        assert_eq!(col.zone_line_at([p[0], p[1], p[2] + 1.0]), Some(7),
            "standing on the projected point must still be INSIDE the region, or the cross never fires");
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

    /// ONE PLAN, ONE BUDGET (#302/#394). The generous clearance pass must take a SLICE of the caller's
    /// NODE budget, never arm a fresh one — two passes each arming the full cap is a plan that quietly
    /// costs two budgets, exactly the disease `PlanCtx` exists to prevent. A node cap, unlike the
    /// wall-clock deadline this replaced, has no "now" to measure from — the split is a plain fraction.
    #[test]
    fn the_generous_pass_takes_a_slice_of_the_budget_never_a_fresh_one() {
        let caller = 1000usize;
        let g = generous_node_cap(Some(caller)).expect("a budgeted plan keeps a budget");
        assert!(g < caller,
            "the generous pass may never get the FULL cap — that is two budgets for one plan (#302)");
        assert_eq!(g, (caller as f32 * GENEROUS_BUDGET_SHARE) as usize,
            "the generous pass gets exactly GENEROUS_BUDGET_SHARE of the caller's node budget");

        // An UNBUDGETED plan (only the MAX_NODES backstop) stays unbudgeted — never invent a cap.
        assert_eq!(generous_node_cap(None), None);
    }

    /// Tiering is a route-CHOICE mechanism, so only a plan that chooses a route pays for it. The
    /// BOUNDED local tier (`max_search: Some`) follows a carrot on a coarse route that was already
    /// chosen with room, inside a 40u window with no meaningful alternative — so it plans at the
    /// MINIMUM clearance and spends its budget on the question it exists to answer: does the
    /// character FIT. Measured on the production call (2u cell / 40u bound / 150 ms, ON the net
    /// thread): the second pass adds ~30-60% mean on top of the sweep and DOUBLES the plans that
    /// overrun the budget (blackburrow 17 -> 30 of 240) while buying nothing (#382).
    #[test]
    fn the_bounded_local_tier_plans_at_the_minimum_clearance_not_the_generous_one() {
        let r = crate::movement::PLAYER_RADIUS;
        let col = slotted_wall(2.0 * r + 0.5); // fits the character, not the preferred margin
        let (start, goal) = ([5.0, 9.0, 0.0], [15.0, 9.0, 0.0]);

        // UNBOUNDED = route-choosing (the coarse planner, off-thread): the generous tier is tried,
        // finds nothing, and the minimum-clearance fallback answers — and REPORTS itself as tight.
        let (s, tight) = col.search_tiered(start, goal, r, &[], 2.0, None, 0.0, PlanCtx::default());
        assert!(matches!(s.path, Some((_, true))), "the route exists at the minimum clearance");
        assert!(tight, "a route that only exists at the minimum must report as tight");

        // BOUNDED = local steering (the net-thread tier): straight to the minimum. No second search,
        // and nothing to call "tight" — no roomier route was ever asked for, so none was denied.
        let (s, tight) = col.search_tiered(start, goal, r, &[], 2.0, Some(60.0), 0.0, PlanCtx::default());
        assert!(matches!(s.path, Some((_, true))),
            "the local tier must still find the route the character fits through");
        assert!(!tight, "a bounded local plan never asks for the generous tier, so it cannot be tight");
    }

    /// A COMPLETE tiered route, or `None` — the `allow_partial = false` question these tests ask.
    /// Returns `(waypoints, tight)`; `tight` = the route only exists at the MINIMUM clearance.
    fn tiered_route(col: &Collision, start: [f32; 3], goal: [f32; 3], radius: f32, cell: f32)
        -> Option<(Vec<[f32; 3]>, bool)> {
        let (s, tight) = col.search_tiered(start, goal, radius, &[], cell, None, 0.0, PlanCtx::default());
        match s.path { Some((p, true)) => Some((p, tight)), _ => None }
    }

    fn slotted_wall(gap: f32) -> Collision {
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // GLB axes -> world: east = p[2], north = p[0], height = p[1].
        let (lo, hi) = (9.0 - gap / 2.0, 9.0 + gap / 2.0);
        let panel = |n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, 10.0], [n1, 0.0, 10.0], [n1, 10.0, 10.0], [n0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets {
            terrain: vec![floor, panel(0.0, lo), panel(hi, 20.0)], objects: vec![], textures: vec![],
        }, 2.0)
    }

    /// #358: the planner validated segments with a RAY while the controller moves a CYLINDER of
    /// `PLAYER_RADIUS`. A 1.5u slot is wider than the ray (which has no width at all) and NARROWER
    /// than the character (2 * PLAYER_RADIUS = 2.0u): the ray threads it, the shoulders do not.
    /// `path_clear` must answer for the character's real volume, not for a line.
    #[test]
    fn path_clear_rejects_a_slot_the_ray_fits_through_but_the_character_does_not() {
        let r = crate::movement::PLAYER_RADIUS;
        let col = slotted_wall(1.5); // < 2 * PLAYER_RADIUS -> the character cannot fit
        let (from, to) = ([5.0, 9.0, 3.0], [15.0, 9.0, 3.0]); // dead down the middle of the slot
        // The LINE really is unobstructed — that is exactly why the old ray test said "clear".
        assert!(col.line_clear(from, to, r), "the centre ray does thread the slot");
        // ...but the character does not fit, so the planner must NOT call this segment clear.
        assert!(!col.path_clear(from, to, r),
            "path_clear must sweep the player's collision volume, not a ray: a {}u slot cannot pass \
             a {}u-radius character", 1.5, r);
        // A slot the character genuinely fits through stays passable — the fix must not seal doors.
        let wide = slotted_wall(2.0 * r + 1.0);
        assert!(wide.path_clear(from, to, r), "a slot wider than the character must stay clear");
    }

    /// The planner must not hand the walker a route through a gap its own collision volume cannot
    /// pass. With the slot as the ONLY way through, the honest answer is "no route" — not a route
    /// the character will wedge in. (Run on the FINE 2u tier: that is the tier that can express a
    /// sub-capsule gap at all, and the tier the walker actually steers along.)
    #[test]
    fn find_path_refuses_a_gap_narrower_than_the_character() {
        let r = crate::movement::PLAYER_RADIUS;
        let narrow = slotted_wall(1.5);
        let route = narrow.find_path_res([5.0, 9.0, 0.0], [15.0, 9.0, 0.0], r, &[], false, 2.0,
            None, 0.0, PlanCtx::default());
        assert!(route.is_none(),
            "A* threaded a gap the character cannot fit through: {route:?}");
        // Control: widen the slot past the character's diameter and the same route appears.
        let wide = slotted_wall(2.0 * r + 1.0);
        let route = wide.find_path_res([5.0, 9.0, 0.0], [15.0, 9.0, 0.0], r, &[], false, 2.0,
            None, 0.0, PlanCtx::default());
        assert!(route.is_some(), "a gap the character DOES fit through must stay routable");
    }

    /// The waypoint inset (#312) must never nudge a waypoint INTO a wall.
    ///
    /// Its original guard, `edge_ok(nudged)`, cannot prevent that — and not by accident:
    /// `column_hits` discards every triangle with `tri_nz <= 0` before intersecting, so a vertical
    /// face is *structurally incapable* of being a `nearest_floor` hit. There is floor at a wall's
    /// foot, so `edge_ok` reports "walkable" right up against one. That was survivable while the
    /// inset was small; the tiered planner now plans at a GENEROUS radius, which scales `margin` and
    /// so the push. A bigger push behind a wall-blind guard walks the character CLOSER to walls than
    /// before — so the push is also checked against the character's own collision volume.
    #[test]
    fn the_waypoint_inset_never_nudges_the_character_into_a_wall() {
        let r = crate::movement::PLAYER_RADIUS;
        // A corridor with a DROP on one side and a WALL on the other. The floor runs out at
        // north 13.5; a wall stands on the floor at north 10. The route runs east between them, so
        // the inset — pushing away from the drop, the only hazard `edge_ok` can see — is aimed
        // squarely at the wall, which it cannot see at all.
        let floor = MeshData { // east 0..40, north 0..12.5
            positions: vec![[0.0, 0.0, 0.0], [12.5, 0.0, 0.0], [12.5, 0.0, 40.0], [0.0, 0.0, 40.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let wall = MeshData { // vertical plane at north = 10, spanning the corridor's whole length
            positions: vec![[10.0, 0.0, 0.0], [10.0, 0.0, 40.0], [10.0, 10.0, 40.0], [10.0, 10.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets {
            terrain: vec![floor, wall], objects: vec![], textures: vec![] }, 8.0);
        // The blind spot itself: there is floor at the wall's FOOT, so the inset's only guard reads
        // "walkable" right up against it. `column_hits` drops every triangle with tri_nz <= 0 before
        // intersecting, so a vertical face can never be a `nearest_floor` hit — by construction.
        assert!(col.nearest_floor(20.0, 10.0, 0.0, 3.0, 8.0).is_some(),
            "nearest_floor is structurally blind to the wall — that is the whole point");

        let path = col.find_path([4.0, 12.0, 0.0], [36.0, 12.0, 0.0], r, &[], true)
            .expect("a route along the corridor exists");
        for p in &path {
            assert!(col.footprint_clear(p[0], p[1], p[2], r, 8),
                "the inset pushed a waypoint into the character's own collision volume at {p:?} — \
                 it nudged away from the drop it CAN see, straight into the wall it CANNOT");
        }
    }

    /// The DIAGONAL leak — found by mutation-testing this very fix, and the reason `path_clear`
    /// samples the whole diameter instead of just the two shoulders.
    ///
    /// A* crossed a wall on a diagonal edge, (9,3) → (11,1), threading a 2.5u slot with a 2.0u
    /// clearance — and all three of the original rays (centre + both shoulders) called it clear. On
    /// a diagonal the shoulders are offset PERPENDICULAR to travel, so they slide ALONG the wall
    /// rather than across it: one starts already past the wall plane, the other never reaches it.
    /// The capsule threads a gap narrower than itself, which is the exact lie #358 exists to kill.
    #[test]
    fn path_clear_does_not_leak_through_a_slot_on_a_diagonal() {
        let col = two_slot_wall(2.5, 6.0); // narrow slot spans north 1.75..4.25
        // The diagonal that leaked: it crosses the wall inside the slot, but a 2.0u-clearance
        // character does not fit through a 2.5u slot.
        assert!(col.line_clear([9.0, 3.0, 3.0], [11.0, 1.0, 3.0], 2.0),
            "the centre line really does thread the slot — that is why the ray test passed it");
        assert!(!col.path_clear([9.0, 3.0, 3.0], [11.0, 1.0, 3.0], 2.0),
            "DIAGONAL LEAK: a 2.0u-clearance character cannot fit a 2.5u slot, on any heading");
        // ...and the orthogonal crossing was already rejected, then and now.
        assert!(!col.path_clear([9.0, 3.0, 3.0], [11.0, 3.0, 3.0], 2.0));
        // The slot IS passable at a clearance it genuinely fits (2.5u slot, 1.0u radius), on the
        // diagonal too — the fix must not seal it.
        assert!(col.path_clear([9.0, 3.0, 3.0], [11.0, 3.0, 3.0], 1.0),
            "a slot the character fits through must stay clear");
    }

    /// The swept-edge cell threshold must actually COVER the tier the walker steers along. These two
    /// numbers live in different modules and were coupled by a comment; a comment does not fail a
    /// build. If `LOCAL_CELL` were raised above `SWEPT_EDGE_MAX_CELL`, the fine tier would silently
    /// fall back to ray clearance and #358 would be un-fixed with every test still green.
    #[test]
    fn the_swept_edge_test_covers_the_tier_the_walker_actually_steers_along() {
        assert!(crate::eq_net::navigation::LOCAL_CELL <= SWEPT_EDGE_MAX_CELL,
            "the local tier ({}) is not covered by the swept edge test (<= {}) — the walker would \
             be steered along ray-validated edges again (#358)",
            crate::eq_net::navigation::LOCAL_CELL, SWEPT_EDGE_MAX_CELL);
        // ...and the coarse whole-zone grid (8u) must stay OUTSIDE it — sweeping an 8u lattice line
        // seals corridors (Ak'Anon: 90/120 routable pairs -> 55/120).
        assert!(SWEPT_EDGE_MAX_CELL < 8.0, "the coarse tier must remain a ray-validated selector");
    }

    /// The coarse/fine asymmetry of `edge_clear` is a deliberate, MEASURED compromise, not an
    /// oversight — pin it so it can't be "tidied" into either extreme. Sweeping the volume on the
    /// coarse 8u lattice seals narrow corridors (Ak'Anon: 90/120 routable pairs → 55/120); casting
    /// a ray on the fine tier is what let the walker be handed the unwalkable route in the first
    /// place.
    #[test]
    fn edge_clear_sweeps_the_volume_at_the_resolution_the_walker_steers_along() {
        let r = crate::movement::PLAYER_RADIUS;
        let col = slotted_wall(1.5); // narrower than the character (2 * PLAYER_RADIUS)
        let (from, to) = ([5.0, 9.0, 3.0], [15.0, 9.0, 3.0]);
        // FINE tier (2u = navigation::LOCAL_CELL): the plan the walker actually steers along. The
        // character's volume must fit, and a corridor here has lateral cells to detour into.
        assert!(!col.edge_clear(from, to, r, 2.0),
            "the fine tier must validate the character's collision VOLUME");
        // COARSE tier (8u): a corridor SELECTOR over a lattice whose centre line is not the walked
        // line — a ray. See `edge_clear` for the routability measurement behind this.
        assert!(col.edge_clear(from, to, r, 8.0),
            "the coarse tier must stay a ray — sweeping an 8u lattice line seals corridors");
    }

    /// A wall at east=10 with TWO ways through: a `narrow` slot centred on north=3 and a `wide` one
    /// centred on north=15. Floor is 20x20 at z=0.
    fn two_slot_wall(narrow: f32, wide: f32) -> Collision {
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let panel = |n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, 10.0], [n1, 0.0, 10.0], [n1, 10.0, 10.0], [n0, 10.0, 10.0]],
            normals: vec![[0.0, 0.0, 1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets {
            terrain: vec![
                floor,
                panel(0.0, 3.0 - narrow / 2.0),                 // ..narrow slot @ north 3..
                panel(3.0 + narrow / 2.0, 15.0 - wide / 2.0),   // ..wide slot @ north 15..
                panel(15.0 + wide / 2.0, 20.0),
            ],
            objects: vec![], textures: vec![],
        }, 2.0)
    }

    /// The owner's requirement: walk with ROOM by default. Given a narrow door and a wide one, the
    /// planner takes the wide one even though the narrow one is closer to the straight line — a
    /// route that skims a wall is a route the walker slides into.
    #[test]
    fn find_path_prefers_the_route_with_room_when_one_exists() {
        let r = crate::movement::PLAYER_RADIUS;
        // Narrow: passable by the character (> 2r) but NOT with the preferred margin (< 2 * 2r).
        // Wide: comfortably passable with the preferred margin.
        let col = two_slot_wall(2.0 * r + 0.5, 2.0 * NAV_PREFERRED_CLEARANCE + 2.0);
        let (path, tight) = tiered_route(&col, [5.0, 3.0, 0.0], [15.0, 3.0, 0.0], r, 2.0)
            .expect("a route exists through both slots");
        assert!(!tight, "a roomy route exists, so the planner must not report a tight one");
        // It detoured NORTH to the wide slot instead of squeezing through the near, narrow one.
        assert!(path.iter().any(|p| p[1] > 10.0),
            "planner squeezed through the narrow slot instead of taking the roomy one: {path:?}");
    }

    /// ...but a narrow door must stay ROUTABLE. When the roomy route does not exist, fall back to the
    /// minimum clearance and say so — a tight route is walkable, just riskier.
    #[test]
    fn find_path_falls_back_to_the_minimum_clearance_when_only_a_tight_route_exists() {
        let r = crate::movement::PLAYER_RADIUS;
        let col = slotted_wall(2.0 * r + 0.5); // fits the character, not the preferred margin
        let before = col.tight_plans();
        let (path, tight) = tiered_route(&col, [5.0, 9.0, 0.0], [15.0, 9.0, 0.0], r, 2.0)
            .expect("a tight door must stay routable — sealing it is a worse bug (#310)");
        assert!(!path.is_empty());
        assert!(tight, "the planner must REPORT that this route only exists at minimum clearance");
        assert_eq!(col.tight_plans(), before + 1,
            "a tight route must be counted so `/v1/observe/debug` can surface `nav_tight` — a \
             degraded mode must never be silent");
    }

    /// #310, the repeat offence: the fallback floor is `PLAYER_RADIUS` and NOTHING gets planned below
    /// it. A gap narrower than the character is not a tight route, it is NO route — and saying so is
    /// the whole point of #358.
    #[test]
    fn find_path_never_plans_below_the_player_radius() {
        let r = crate::movement::PLAYER_RADIUS;
        let col = slotted_wall(2.0 * r - 0.5); // narrower than the character
        for asked in [r, r * 0.5, r * 0.25, 0.0] {
            let route = tiered_route(&col, [5.0, 9.0, 0.0], [15.0, 9.0, 0.0], asked, 2.0);
            assert!(route.is_none(),
                "radius={asked}: planner threaded a gap the character cannot fit through. The \
                 minimum clearance is PLAYER_RADIUS ({r}) and a caller asking for less does not \
                 lower it (#310).");
        }
    }

    /// The OTHER hazard. `edge_clear` sees geometry that is in the way; only `ground_margin_ok` sees
    /// geometry that is MISSING. Route around the inside corner of a sheer drop and require the
    /// route to keep its standing room from the brink — "walking near edges is a good way to fall
    /// off them".
    #[test]
    fn find_path_keeps_its_standing_room_from_a_drop() {
        // Floor is an L: east 0..8 (all north), plus east 8..20 for north 12..20.
        // The rest — east 8..20, north 0..12 — is a VOID the character would fall into.
        let quad = |e0: f32, e1: f32, n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e0], [n1, 0.0, e1], [n0, 0.0, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets {
            terrain: vec![quad(0.0, 8.0, 0.0, 20.0), quad(8.0, 20.0, 12.0, 20.0)],
            objects: vec![], textures: vec![],
        }, 2.0);
        assert!(!col.ground_margin_ok(9.0, 13.0, 0.0, NAV_PREFERRED_CLEARANCE),
            "a point 1u from the brink has no standing room");
        assert!(col.ground_margin_ok(12.0, 17.0, 0.0, NAV_PREFERRED_CLEARANCE),
            "a point well inside the floor does");

        // The straight line (4,4) -> (16,16) runs clean through the void, so A* must round the
        // inside corner at (8,12) — the exact place a route hugs a drop.
        let (path, _) = tiered_route(&col, [4.0, 4.0, 0.0], [16.0, 16.0, 0.0],
            crate::movement::PLAYER_RADIUS, 2.0).expect("a route around the void exists");
        // Every waypoint the walker is asked to stand on has a body-width of ground around it.
        // (The final waypoint is snapped to the caller's exact goal, which is the caller's problem.)
        for p in &path[..path.len() - 1] {
            assert!(col.ground_margin_ok(p[0], p[1], p[2], NAV_PREFERRED_CLEARANCE),
                "route hugs the brink of the drop at {p:?} — no standing room");
        }
    }

    /// The case the waypoint inset CANNOT rescue, and therefore the case that justifies testing the
    /// ledge margin inside the SEARCH rather than nudging waypoints afterwards.
    ///
    /// Two platforms joined by a short 3u CATWALK and a long 8u BRIDGE. The catwalk is wide enough
    /// for the character (> 2 · PLAYER_RADIUS) and it is the direct line — but standing on it puts a
    /// sheer drop barely a step away on both sides. The inset cannot fix that: nudging away from one
    /// brink walks into the other, so the pushes cancel and the waypoint stays on the brink. Only the
    /// search can fix it, by going round.
    #[test]
    fn find_path_takes_the_long_wide_bridge_over_the_short_narrow_catwalk() {
        let r = crate::movement::PLAYER_RADIUS;
        let quad = |e0: f32, e1: f32, n0: f32, n1: f32| MeshData {
            positions: vec![[n0, 0.0, e0], [n1, 0.0, e0], [n1, 0.0, e1], [n0, 0.0, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets {
            terrain: vec![
                quad(0.0, 10.0, 0.0, 20.0),    // platform A
                quad(20.0, 30.0, 0.0, 20.0),   // platform B
                quad(10.0, 20.0, 10.0, 13.0),  // CATWALK: 3u wide, on the direct line
                quad(10.0, 20.0, 0.0, 6.0),    // BRIDGE: 6u wide, a detour to the south
                                               // (void between them: north 6..10)
            ],
            objects: vec![], textures: vec![],
        }, 2.0);
        // The catwalk fits the character but has no standing room; the bridge has both.
        assert!(col.ground_margin_ok(15.0, 11.5, 0.0, r), "the catwalk fits the character");
        assert!(!col.ground_margin_ok(15.0, 11.5, 0.0, NAV_PREFERRED_CLEARANCE),
            "...but there is a drop a step away on both sides");
        assert!(col.ground_margin_ok(15.0, 3.0, 0.0, NAV_PREFERRED_CLEARANCE), "the bridge has room");

        let (path, tight) = tiered_route(&col, [5.0, 11.0, 0.0], [25.0, 11.0, 0.0], r, 2.0)
            .expect("both crossings exist");
        assert!(!tight, "a roomy crossing exists, so the route must not be a tight one");
        // It took the BRIDGE, not the direct catwalk.
        assert!(path.iter().any(|p| p[1] < 6.0),
            "planner walked the brink of the catwalk instead of detouring to the wide bridge — and \
             the waypoint inset cannot save it (both sides are a drop, so the nudges cancel): {path:?}");
        for p in &path[..path.len() - 1] {
            assert!(col.ground_margin_ok(p[0], p[1], p[2], NAV_PREFERRED_CLEARANCE),
                "route has no standing room at {p:?}");
        }
        // ...and when the catwalk is the ONLY crossing, it stays routable — as a TIGHT route.
        let only_catwalk = Collision::build(&ZoneAssets {
            terrain: vec![
                quad(0.0, 10.0, 0.0, 20.0), quad(20.0, 30.0, 0.0, 20.0), quad(10.0, 20.0, 10.0, 13.0),
            ],
            objects: vec![], textures: vec![],
        }, 2.0);
        let (_, tight) = tiered_route(&only_catwalk, [5.0, 11.0, 0.0], [25.0, 11.0, 0.0], r, 2.0)
            .expect("the only crossing must stay routable (#310) — sealing it is the worse bug");
        assert!(tight, "a catwalk-only crossing is walkable, but it must REPORT as tight");
    }

    /// #358 drift guard: the clearance the PLANNER validates with and the radius the CONTROLLER
    /// moves with are the same number, and the waypoint inset (#312) is never smaller than it.
    /// An inset below the collision radius puts the capsule's shoulder inside the wall by
    /// construction — which is what the old `.min(cell * 0.45)` clamp did on the fine 2u tier

    /// The inset must deliver its margin from BOTH walls at an inside corner. A normalised diagonal
    /// push spends the margin on the diagonal and leaves only `margin / √2` per wall — under the

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
    fn find_path_edge_margin_keeps_waypoints_on_mesh() {
        // #312 safety: the waypoint edge-inset must never shove a point OFF the floor (it only nudges
        // toward walkable interior, guarded by `edge_ok(nudged)`). Route across the same 20x20 floor
        // the wall test uses and assert every waypoint stays on real floor. (The real edge-hug fix is
        // validated live against #314; a flat synthetic floor is already covered by path_clear.)
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [20.0, 0.0, 0.0], [20.0, 0.0, 20.0], [0.0, 0.0, 20.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 2.0);
        let path = col.find_path([3.0, 3.0, 0.0], [17.0, 17.0, 0.0], 1.0, &[], false)
            .expect("a route across the floor should exist");
        for p in &path {
            assert!(col.nearest_floor(p[0], p[1], 0.0, 3.0, 8.0).is_some(),
                "edge-inset pushed a waypoint off-mesh: {p:?}");
        }
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
        // Wound UP-FACING (same vertex order as `slab`) — these are FLOORS, which is what the
        // `normals: [0,1,0]` below has always claimed. The winding used to be reversed here, so
        // every "floor" in this fixture was really a down-facing face; the test only passed because
        // an all-inverted mesh failed the old whole-zone winding gate, which switched the
        // floor-normal filter off entirely. With the filter always on, a floor has to be wound like
        // one.
        let quad = |n0: f32, n1: f32, e0: f32, e1: f32, up: f32| MeshData {
            positions: vec![[n0, up, e0], [n0, up, e1], [n1, up, e1], [n1, up, e0]],
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
    fn aggro_buffer_widens_the_berth_around_npcs() {
        // #242: a larger `aggro_buffer` on find_path_res gives the NPC MORE berth (route bows wider),
        // while still reaching the goal — the avoidance stays soft (never fails).
        let floor = MeshData {
            positions: vec![[0.0, 0.0, 0.0], [200.0, 0.0, 0.0], [200.0, 0.0, 200.0], [0.0, 0.0, 200.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        let col = Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 16.0);
        let start = [20.0, 100.0, 0.0];
        let goal  = [180.0, 100.0, 0.0];
        let npc = [[100.0, 100.0f32]];
        let min_to_npc = |path: &[[f32; 3]]| path.iter()
            .map(|w| ((w[0] - npc[0][0]).powi(2) + (w[1] - npc[0][1]).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);

        let narrow = col.find_path_res(start, goal, 1.0, &npc, false, 8.0, None, 0.0, PlanCtx::default()).expect("route exists");
        let wide   = col.find_path_res(start, goal, 1.0, &npc, false, 8.0, None, 60.0, PlanCtx::default()).expect("wider route still exists");

        assert!(min_to_npc(&wide) > min_to_npc(&narrow) + 8.0,
            "a bigger aggro_buffer should widen the berth (wide {} vs narrow {})",
            min_to_npc(&wide), min_to_npc(&narrow));
        let last = *wide.last().unwrap();
        assert!((last[0] - goal[0]).abs() < 3.0 && (last[1] - goal[1]).abs() < 3.0, "still reaches goal: {last:?}");
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

    /// **PICK THE NODE CAP BY MEASUREMENT (#394).** The worker's node cap must be generous enough that
    /// a legitimate WHOLE-ZONE "no route" still reaches `SearchClosed` — a cap that truncates a real
    /// full-frontier close into `Exhausted(NodeCap)` would be a new honesty bug ("I don't know" where
    /// the truth is "no route"). So this measures the LARGEST reachable component across the MEASURED
    /// corpus (the baked zones available locally — not all of RoF2; see the `MAX_NODES` caveat): for
    /// each zone, a start on real floor routed to far corners that force A* to flood the reachable
    /// component. `closed_n` at that point IS the component size — the worst case the cap must clear.
    /// Run with a HIGH cap so nothing truncates.
    ///
    /// ```text
    /// cargo test --release --lib worst_case_reachable_component -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs; measurement for the #394 node cap"]
    fn worst_case_reachable_component() {
        use std::time::Instant;
        let dir = format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap());
        // The biggest grids first — those are where a full close is largest.
        let zones = ["everfrost", "butcher", "gfaydark"];
        println!("\n{:<12} {:>12} {:>12} {:>10}", "zone", "xy_cells@8u", "MAX_closed", "ms");
        let mut worst = 0usize;
        for zone in zones {
            let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
            let Ok(za) = ZoneAssets::from_glb(&p) else { continue };
            let col = Collision::build(&za, 32.0);
            if col.cols == 0 { continue; }
            let mut seed: u64 = 99;
            let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
            // Sample several starts; force each to an OFF-MESH goal (far outside the grid, no floor)
            // so A* must close the whole reachable component. Take the largest closed_n seen.
            let (mut max_closed, mut max_ms) = (0usize, 0u128);
            let ext_e = col.cols as f32 * col.cell_size;
            let ext_n = col.rows as f32 * col.cell_size;
            // The four grid corners, on real floor: a route to the FARTHEST reachable floor explores
            // the largest span of the component. Whether it reaches or closes, `closed_n` at a HIGH cap
            // is the worst-case number the node cap must clear.
            let corners = [(0.05, 0.05), (0.95, 0.05), (0.05, 0.95), (0.95, 0.95), (0.5, 0.5)];
            for _ in 0..120 {
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * ext_e;
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * ext_n;
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                // corners + one fully-random goal, so we don't systematically miss a hard interior pair
                let rc = (rnd() as f32 / u32::MAX as f32, rnd() as f32 / u32::MAX as f32);
                let probes = corners.iter().copied().chain(std::iter::once(rc));
                for (fe, fn_) in probes {
                    let (ge, gn) = (col.origin[0] + fe * ext_e, col.origin[1] + fn_ * ext_n);
                    let Some(gz) = col.nearest_floor(ge, gn, col.z_max, 10.0, 4000.0) else { continue };
                    let t = Instant::now();
                    let (sr, _t) = col.search_tiered_for_test(
                        [e, n, z], [ge, gn, gz], crate::movement::PLAYER_RADIUS, &[], 8.0, None, 0.0,
                        PlanCtx { node_cap: Some(8_000_000), ..PlanCtx::default() });
                    let ms = t.elapsed().as_millis();
                    if sr.closed_n > max_closed { max_closed = sr.closed_n; max_ms = ms; }
                }
            }
            worst = worst.max(max_closed);
            let xy = (col.cols as f32 * col.cell_size / 8.0).ceil() * (col.rows as f32 * col.cell_size / 8.0).ceil();
            println!("{zone:<12} {:>12.0} {:>12} {:>10}", xy, max_closed, max_ms);
        }
        println!("\nWORST reachable-component close across corpus: {worst} nodes");
        println!("=> the coarse MAX_NODES backstop must be comfortably above this (it is: 8M).");
    }

    /// **THE #382 CORPUS MEASUREMENT.** Fine-tier route success and cost, OLD (inline, net-thread) vs
    /// NEW (`find_path_local`, off-thread), over real baked zones — because every previous tightening of
    /// nav has SEALED ZONES (the coarse capsule sweep cost −29% route success in akanon, see
    /// `path_clear`), so "it should be strictly better" is not good enough to ship on.
    ///
    /// Note the baseline is NOT a wall clock: #394 already deleted that. Both sides use the SAME
    /// node-capped search — the only difference #382 makes is WHERE it runs (net thread vs worker) and
    /// that `find_path_local` returns an HONEST `LocalOutcome` instead of a bare `Option`. So this gate
    /// proves the new API + off-thread move does not change which carrots get threaded.
    ///
    /// * **OLD** = `find_path_res(.., allow_partial: true, .., PlanCtx::net_tier())` — verbatim what
    ///   `navigation.rs` called inline on the network thread before this change.
    /// * **NEW** = `find_path_local(..)` — the same node-capped search, off the net thread.
    ///
    /// Both are run back-to-back on identical (start, carrot) pairs sampled the way production
    /// generates them: walk a real coarse route and take carrots `LOCAL_REACH` (24 u) ahead of points
    /// along it. Reports threaded-count, disagreements, and the timing distribution — the same
    /// per-tick cost that used to land on the net thread.
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test --lib fine_tier_corpus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR"]
    fn fine_tier_corpus_route_success_and_cost() {
        use std::time::Instant;
        const LOCAL_REACH: f32 = 24.0;
        const LOCAL_BOUND: f32 = 40.0;
        const LOCAL_CELL:  f32 = 2.0;

        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec![
                // A deliberately mixed corpus: the zone #382's own numbers came from (akanon), the one
                // whose route success the last nav tightening cost 29% (akanon again), a dense city, a
                // big outdoor zone, dungeons, and the gfaydark corner an earlier budget cut broke.
                "akanon", "blackburrow", "qeynos2", "gfaydark", "crushbone", "neriaka", "felwithea",
                "highpass", "everfrost", "butcher",
            ].into_iter().map(str::to_string).collect());

        // A seeded LCG: a failure here must be reproducible, and an unseeded sample is not evidence.
        let mut seed: u64 = 0x3820_0F1E;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        let (mut tot_old_ok, mut tot_new_ok, mut tot_pairs) = (0usize, 0usize, 0usize);
        let (mut tot_new_only, mut tot_old_only) = (0usize, 0usize);
        let mut old_us: Vec<u128> = Vec::new();
        let mut new_us: Vec<u128> = Vec::new();

        println!("\n{:<12} {:>6} {:>10} {:>10} {:>9} {:>9} {:>10} {:>10}",
            "zone", "pairs", "old ok", "new ok", "new-only", "old-only", "old mean", "new mean");
        for zone in &zones {
            let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
            let Ok(za) = ZoneAssets::from_glb(&p) else { println!("{zone:<12}  (no glb — skipped)"); continue };
            let col = Collision::build(&za, 32.0);
            if col.cols == 0 { println!("{zone:<12}  (no grid — skipped)"); continue; }

            // Sample (start, carrot) pairs the way production makes them: real coarse routes, carrots
            // 24u ahead along them. A carrot invented out of thin air would not be the question the
            // fine tier is actually asked.
            let mut pairs: Vec<([f32; 3], [f32; 3])> = Vec::new();
            let mut tries = 0;
            while pairs.len() < 240 && tries < 900 {
                tries += 1;
                // A random point on walkable floor...
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * (col.rows as f32 * col.cell_size);
                // Anchor the probe at the TOP of the zone and search down. NOT at the midpoint of
                // [z_min, z_max]: several zones (gfaydark, everfrost, butcher) carry invisible-boundary
                // art at z ~= -32768, which drags the midpoint 16,000 units below the world and made the
                // first version of this sampler find NO floor at all in exactly the big outdoor zones
                // that matter most here (gfaydark is the zone a tighter fine-tier budget once broke).
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                let s = [e, n, z];
                // ...and a goal 120-400u away in a random direction. (Two INDEPENDENT random points in
                // a big outdoor zone are almost never mutually routable, which is why the first version
                // of this sampler produced zero pairs for gfaydark/everfrost/butcher and only 7 for
                // akanon — the very zone #382's numbers came from. A displaced goal is both far more
                // productive and a better model of what an agent actually asks for.)
                let ang = (rnd() as f32 / u32::MAX as f32) * std::f32::consts::TAU;
                let d = 120.0 + (rnd() as f32 / u32::MAX as f32) * 280.0;
                let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
                let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
                // A real coarse route (8u, the off-thread contract), then carrots along it.
                let PlanOutcome::Route(route) = col.find_path_ex(
                    s, [ge, gn, gz], crate::movement::PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker())
                    else { continue };
                if route.len() < 3 { continue; }
                // Take a spread of carrots along the route, not just its head, so the sample covers the
                // whole journey (corners, doorways, stairs) rather than only its easy first stride.
                for i in (0..route.len().saturating_sub(2)).step_by(3) {
                    if pairs.len() >= 240 { break; }
                    let from = route[i];
                    let Some(carrot) = crate::eq_net::navigation::carrot_along(&route, i, [from[0], from[1]], LOCAL_REACH)
                        else { continue };
                    pairs.push((from, carrot));
                }
            }
            if pairs.is_empty() { println!("{zone:<12}  (no routable pairs — skipped)"); continue; }

            let (mut old_ok, mut new_ok, mut new_only, mut old_only) = (0usize, 0usize, 0usize, 0usize);
            let (mut zo, mut zn) = (Vec::new(), Vec::new());
            for (s, c) in &pairs {
                // OLD: exactly the call navigation.rs made inline on the net thread (node-capped).
                let t0 = Instant::now();
                let old = col.find_path_res(*s, *c, crate::movement::PLAYER_RADIUS, &[], true,
                    LOCAL_CELL, Some(LOCAL_BOUND), 0.0, PlanCtx::net_tier());
                let ot = t0.elapsed().as_micros();
                // The old API cannot say whether it reached the carrot — it returns partials as routes.
                // That is the #382 honesty gap. Reconstruct "reached" the way the walker had to: measure.
                let o_reached = old.as_ref().and_then(|p| p.last()).is_some_and(|w|
                    (w[0] - c[0]).hypot(w[1] - c[1]) <= LOCAL_CELL * 2.0);

                // NEW: the same node-capped search, off the net thread, with an outcome that says which
                // answer it is (threaded / no-way-through / exhausted).
                let t1 = Instant::now();
                let new = col.find_path_local(*s, *c, LOCAL_CELL, LOCAL_BOUND, LOCAL_CELL * 2.0);
                let nt = t1.elapsed().as_micros();

                if o_reached { old_ok += 1; }
                if new.threaded() { new_ok += 1; }
                if new.threaded() && !o_reached { new_only += 1; }
                if o_reached && !new.threaded() { old_only += 1; }
                zo.push(ot); zn.push(nt);
            }
            let mean = |v: &[u128]| if v.is_empty() { 0 } else { (v.iter().sum::<u128>() / v.len() as u128) as usize };
            println!("{zone:<12} {:>6} {:>9}  {:>9}  {:>8}  {:>8}  {:>8}us {:>8}us",
                pairs.len(), old_ok, new_ok, new_only, old_only, mean(&zo), mean(&zn));
            tot_pairs += pairs.len(); tot_old_ok += old_ok; tot_new_ok += new_ok;
            tot_new_only += new_only; tot_old_only += old_only;
            old_us.extend(zo); new_us.extend(zn);
        }

        old_us.sort_unstable(); new_us.sort_unstable();
        let pct = |v: &[u128], p: usize| if v.is_empty() { 0 } else { v[(v.len() - 1) * p / 100] };
        let mean = |v: &[u128]| if v.is_empty() { 0 } else { (v.iter().sum::<u128>() / v.len() as u128) as usize };
        println!("\n=== FINE-TIER CORPUS ({tot_pairs} pairs) ===");
        println!("route success  OLD (inline, net-thread) : {tot_old_ok}/{tot_pairs} = {:.2}%",
            100.0 * tot_old_ok as f32 / tot_pairs as f32);
        println!("route success  NEW (off-thread worker)  : {tot_new_ok}/{tot_pairs} = {:.2}%",
            100.0 * tot_new_ok as f32 / tot_pairs as f32);
        println!("  NEW threads what OLD could not : {tot_new_only}");
        println!("  OLD threads what NEW could not : {tot_old_only}   <-- MUST BE 0 (a regression)");
        println!("cost/plan  OLD (inline/net-thread)  mean {}us  p50 {}us  p99 {}us  max {}us",
            mean(&old_us), pct(&old_us, 50), pct(&old_us, 99), old_us.last().copied().unwrap_or(0));
        println!("cost/plan  NEW  mean {}us  p50 {}us  p99 {}us  max {}us  (paid on the fine WORKER, not the net thread)",
            mean(&new_us), pct(&new_us, 50), pct(&new_us, 99), new_us.last().copied().unwrap_or(0));

        assert!(tot_pairs > 0, "the corpus produced no pairs — check $ZONE_DIR");
        // THE REGRESSION GATE. Deleting a deadline can only ADD completed searches: any search that
        // finished inside 150ms finishes identically without it (A* is deterministic given the same
        // inputs; the deadline only ever ABORTS). So a route the old tier threaded and the new one
        // cannot is not a rounding difference — it is a real regression, and this must catch it.
        assert_eq!(tot_old_only, 0,
            "REGRESSION: {tot_old_only} carrots the OLD budgeted fine tier could thread and the new one \
             cannot. Deleting the wall clock must be monotone.");
        assert!(tot_new_ok >= tot_old_ok, "fine-tier route success must not go down");
    }

    /// **THE FAITHFUL WALKER DRIFT SCANNER (the real per-tick recovery loop).** The static scanner
    /// above drove ONE fine plan with naive pure pursuit and no recovery — which over-counts corner
    /// wedges the real walker recovers from, and cannot measure a planner-cell fix's benefit (the real
    /// walker re-anchors its fine plan every tick, so cleaner cells help it even when a single static
    /// plan wedges). This one mirrors `navigation.rs`'s ACTUAL two-rate loop (post-#399):
    ///
    ///   * a COARSE route committed at goal-change (`find_path_ex`), re-planned on stall/backoff;
    ///   * a ~100 Hz FAST-STEER aim: `fast_steer_aim` toward a 5u carrot on `local_path` (cursor
    ///     `local_i`), refreshed EVERY controller frame — the thing that hugs a bend;
    ///   * a 150 ms NAV TICK that advances `path_i`, RE-POSTS a fresh `find_path_local` from the
    ///     walker's CURRENT position (1-tick lag, as #399's worker introduces), and runs stall
    ///     detection → downhill backoff → coarse re-plan (capped at 8 attempts), plus the #246/#379
    ///     proactive coarse re-plan when the fine tier reports `NoWayThrough`.
    ///
    /// Then it classifies terminal wedges (never arrived, 8 re-paths spent) by face. THIS is the
    /// number that gates a planner-cell fix — run it before/after PR-B.
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test --release --lib faithful_walker_drift_corpus -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; the faithful per-tick-recovery drift baseline"]
    fn faithful_walker_drift_corpus() {
        use crate::eq_net::navigation::{carrot_along, fast_steer_aim};
        use crate::movement::{CharacterController, MoveIntent, PLAYER_RADIUS};

        // Production constants, verbatim from navigation.rs (kept in sync — if these drift, the scanner
        // stops modelling the real walker).
        const RUN_SPEED: f32 = 44.0;
        const LOOK_AHEAD: f32 = 5.0;
        const LOCAL_REACH: f32 = 24.0;
        const LOCAL_BOUND: f32 = 40.0;
        const LOCAL_CELL:  f32 = 2.0;
        const NAV_STUCK_TICKS: u32 = 20;
        const NAV_HOP_TICKS: u32 = 6;
        const NAV_BACKOFF_TICKS: u32 = 3;
        const NAV_LOCAL_STUCK_TICKS: u32 = 3;
        const REPLAN_COOLDOWN_TICKS: u32 = 6;
        const MAX_REPATHS: u32 = 8;
        const DT: f32 = 1.0 / 100.0;          // ~100 Hz controller, per navigation.rs's fast-steer note
        const FRAMES_PER_TICK: u32 = 15;      // 150 ms / 10 ms

        // The faithful walk. Returns None on arrival, or Some(wedge_pos) on a terminal wedge.
        let simulate = |col: &Collision, start: [f32; 3], goal: [f32; 3]| -> Option<([f32; 3], [f32; 2])> {
            let PlanOutcome::Route(mut coarse) = col.find_path_ex(
                start, goal, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) else { return None };
            if coarse.len() < 2 { return None; }
            let mut ctrl = CharacterController::new(start);
            ctrl.on_ground = true;
            let mut path_i = 0usize;
            let mut local_path: Vec<[f32; 3]> = Vec::new();
            let mut local_i = 0usize;
            // Fine plan requested LAST tick, applied THIS tick (models #399's ~1-tick worker lag).
            let mut pending_local: Option<Vec<[f32; 3]>> = None;
            let mut pending_nwt = false;
            let (mut stuck_i, mut stuck_ticks, mut repaths) = (0usize, 0u32, 0u32);
            let (mut local_stuck, mut replan_cd) = (0u32, 0u32);
            let (mut backoff_ticks, mut backoff_dir) = (0u32, [0.0f32, 0.0]);
            let mut aim = [0.0f32, 0.0];

            // A journey either arrives, or spends its 8 re-paths (~8·NAV_STUCK_TICKS ticks) and wedges.
            // 200 ticks (~30 s sim) is well past both for a ≤400u route at RUN_SPEED — a journey still
            // going at 200 is not making progress and counts as wedged.
            let nav_ticks_budget = 200;
            for _ in 0..nav_ticks_budget {
                let (px, py) = (ctrl.pos[0], ctrl.pos[1]);
                // ── arrival on the FINAL goal ──
                if (px - goal[0]).hypot(py - goal[1]) < 3.0 { return None; }

                // ── the 150 ms NAV TICK (planning / recovery) ──
                // advance path_i along the coarse route
                while path_i + 2 < coarse.len() {
                    let (a, b) = (coarse[path_i], coarse[path_i + 1]);
                    let ab = [b[0] - a[0], b[1] - a[1]];
                    let l2 = ab[0] * ab[0] + ab[1] * ab[1];
                    let t = if l2 < 1e-6 { 1.0 } else { ((px - a[0]) * ab[0] + (py - a[1]) * ab[1]) / l2 };
                    if t >= 1.0 { path_i += 1; } else { break; }
                }
                if replan_cd > 0 { replan_cd -= 1; }

                // downhill backoff in progress → drive reverse aim, then re-plan when it ends
                if backoff_ticks > 0 {
                    backoff_ticks -= 1;
                    for _ in 0..FRAMES_PER_TICK {
                        ctrl.step(MoveIntent { wish_dir: backoff_dir, wish_vspeed: 0.0, jump: false,
                            want_swim: false, speed: RUN_SPEED, climb: 0.0, hop: false }, DT, col);
                    }
                    if backoff_ticks == 0 {
                        if let PlanOutcome::Route(r) = col.find_path_ex(
                            [ctrl.pos[0], ctrl.pos[1], ctrl.pos[2]], goal, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) {
                            coarse = r; path_i = 0; local_path.clear(); local_i = 0;
                        }
                        stuck_ticks = 0;
                    }
                    continue;
                }

                // apply the fine plan requested last tick (1-tick lag)
                if let Some(lp) = pending_local.take() {
                    local_path = lp; local_i = 0;
                    if pending_nwt {
                        local_stuck += 1;
                        if local_stuck >= NAV_LOCAL_STUCK_TICKS && replan_cd == 0 {
                            if let PlanOutcome::Route(r) = col.find_path_ex(
                                [px, py, ctrl.pos[2]], goal, PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) {
                                coarse = r; path_i = 0; local_path.clear(); local_i = 0;
                            }
                            local_stuck = 0; replan_cd = REPLAN_COOLDOWN_TICKS;
                        }
                    } else {
                        local_stuck = 0;
                    }
                    // pending_nwt is reassigned by the match below every tick, no reset needed here.
                }
                // post a fresh fine plan for NOW (lands next tick)
                let coarse_carrot = carrot_along(&coarse, path_i, [px, py], LOCAL_REACH)
                    .unwrap_or([goal[0], goal[1], ctrl.pos[2]]);
                match col.find_path_local([px, py, ctrl.pos[2]], coarse_carrot, LOCAL_CELL, LOCAL_BOUND, LOCAL_CELL * 2.0) {
                    LocalOutcome::Threaded(s)     => { pending_local = Some(s); pending_nwt = false; }
                    LocalOutcome::NoWayThrough{steer, ..} => { pending_local = Some(steer); pending_nwt = true; }
                    LocalOutcome::Exhausted{steer, ..}    => { pending_local = Some(steer); pending_nwt = false; }
                }

                // stall detection on coarse path_i progress
                if path_i > stuck_i { stuck_i = path_i; stuck_ticks = 0; }
                else {
                    stuck_ticks += 1;
                    if stuck_ticks >= NAV_STUCK_TICKS {
                        stuck_ticks = 0;
                        if repaths < MAX_REPATHS {
                            repaths += 1;
                            backoff_ticks = NAV_BACKOFF_TICKS;
                            let carrot = carrot_along(&coarse, path_i, [px, py], LOOK_AHEAD)
                                .unwrap_or([goal[0], goal[1], ctrl.pos[2]]);
                            let (dx, dy) = (carrot[0] - px, carrot[1] - py);
                            let dl = (dx * dx + dy * dy).sqrt();
                            backoff_dir = if dl > 1e-3 { [-dx / dl, -dy / dl] } else { [0.0, 0.0] };
                            continue;
                        }
                        return Some((ctrl.pos, aim)); // terminal wedge (8 re-paths spent)
                    }
                }

                // ── the ~100 Hz FAST-STEER + controller stepping for this tick ──
                for _ in 0..FRAMES_PER_TICK {
                    let from = [ctrl.pos[0], ctrl.pos[1]];
                    // fast-steer aim on the fine plan if present, else the coarse carrot
                    let steer_aim = if local_path.len() >= 2 {
                        fast_steer_aim(&local_path, &mut local_i, from, LOOK_AHEAD).map(|(d, _)| d)
                    } else { None };
                    aim = steer_aim.unwrap_or_else(|| {
                        let c = carrot_along(&coarse, path_i, from, LOOK_AHEAD)
                            .unwrap_or([goal[0], goal[1], ctrl.pos[2]]);
                        let (dx, dy) = (c[0] - from[0], c[1] - from[1]);
                        let d = (dx * dx + dy * dy).sqrt().max(1e-3);
                        [dx / d, dy / d]
                    });
                    ctrl.step(MoveIntent { wish_dir: aim, wish_vspeed: 0.0, jump: false, want_swim: false,
                        speed: RUN_SPEED, climb: 0.0, hop: stuck_ticks >= NAV_HOP_TICKS }, DT, col);
                    if (ctrl.pos[0] - goal[0]).hypot(ctrl.pos[1] - goal[1]) < 3.0 { return None; }
                }
            }
            Some((ctrl.pos, aim)) // ran out of sim time
        };

        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec![
                "akanon", "blackburrow", "qeynos2", "gfaydark", "crushbone", "neriaka", "felwithea",
                "highpass", "everfrost", "butcher",
            ].into_iter().map(str::to_string).collect());

        let mut seed: u64 = 0xD21F_7A3E; // same seed family as the static scanner
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        let (mut tot_pairs, mut tot_walked, mut tot_wedged) = (0usize, 0usize, 0usize);
        let (mut tot_height, mut tot_overlap, mut tot_other) = (0usize, 0usize, 0usize);
        println!("\n{:<12} {:>6} {:>7} {:>8} {:>8} {:>6}", "zone", "walked", "wedged", "height", "overlap", "other");
        for zone in &zones {
            let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
            let Ok(za) = ZoneAssets::from_glb(&p) else { println!("{zone:<12}  (no glb — skipped)"); continue };
            let mut col = Collision::build(&za, 32.0);
            if col.cols == 0 { println!("{zone:<12}  (no grid — skipped)"); continue; }
            col.set_water(crate::region_map::RegionMap::load(&std::path::Path::new(&dir).join("maps/water"), zone).map(std::sync::Arc::new));

            // Sample full (start, goal) pairs: a random floor point and a goal 120-400u away that a
            // coarse route actually reaches (so we simulate real journeys, not un-routable noise).
            let mut pairs: Vec<([f32; 3], [f32; 3])> = Vec::new();
            let mut tries = 0;
            while pairs.len() < 60 && tries < 2000 {
                tries += 1;
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * (col.cols as f32 * col.cell_size);
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * (col.rows as f32 * col.cell_size);
                let Some(z) = col.nearest_floor(e, n, col.z_max, 10.0, 4000.0) else { continue };
                let ang = (rnd() as f32 / u32::MAX as f32) * std::f32::consts::TAU;
                let d = 120.0 + (rnd() as f32 / u32::MAX as f32) * 280.0;
                let (ge, gn) = (e + d * ang.cos(), n + d * ang.sin());
                let Some(gz) = col.nearest_floor(ge, gn, z, 400.0, 400.0) else { continue };
                // Dry routes only (the swim-capable variant, PR-D, un-skips water).
                let s = [e, n, z]; let g = [ge, gn, gz];
                if col.in_water(s) || col.in_water(g) { continue; }
                // DRIVABILITY FILTER. This pure-pursuit sim faithfully drives WALK legs only. It does
                // NOT execute A*'s controlled-fall, jump-edge, or swim edges (those need the walker's
                // fall/jump/swim intents, out of scope here — the static scanner skipped them per-PLAN
                // for the same reason). So only accept a journey whose COARSE route is all-walkable: no
                // segment with a big z-drop (controlled fall / jump landing) and no waypoint in water.
                // Without this, multi-level dungeons (blackburrow, neriaka) flood the count with wedges
                // at fall/swim TRANSITIONS the sim structurally cannot cross — a sim artifact, not a
                // walker drift. (PR-D's swim-capable variant will drive the water legs.)
                let PlanOutcome::Route(cr) = col.find_path_ex(
                    s, g, crate::movement::PLAYER_RADIUS, &[], 8.0, None, 0.0, PlanCtx::worker()) else { continue };
                if cr.len() < 3 { continue; }
                let drivable = cr.windows(2).all(|w| {
                    let dz = w[1][2] - w[0][2];
                    let seg = (w[1][0] - w[0][0]).hypot(w[1][1] - w[0][1]);
                    dz > -4.0 && seg < 12.0 // no controlled fall, no jump-edge span
                }) && !cr.iter().any(|w| col.in_water(*w) || col.in_water([w[0], w[1], w[2] + 3.0]));
                if !drivable { continue; }
                pairs.push((s, g));
            }
            if pairs.is_empty() { println!("{zone:<12}  (no routable pairs — skipped)"); continue; }

            let (mut walked, mut wedged, mut n_h, mut n_o, mut n_x) = (0usize, 0usize, 0usize, 0usize, 0usize);
            for (s, g) in &pairs {
                walked += 1;
                let Some((w, aim)) = simulate(&col, *s, *g) else { continue };
                // classify: the plan may route through water mid-journey; skip a wedge that ended in
                // water (out of scope until the swim variant).
                if col.in_water(w) || col.in_water([w[0], w[1], w[2] + 3.0]) { walked -= 1; continue; }
                wedged += 1;
                let to = [w[0] + aim[0] * 4.0, w[1] + aim[1] * 4.0];
                let ctrl_chest_blocked = !col.line_clear([w[0], w[1], w[2] + 4.0], [to[0], to[1], w[2] + 4.0], PLAYER_RADIUS);
                let planner_clear =
                    col.path_clear([w[0], w[1], w[2] + 3.0], [to[0], to[1], w[2] + 3.0], PLAYER_RADIUS)
                    && col.path_clear([w[0], w[1], w[2] + 2.5], [to[0], to[1], w[2] + 2.5], PLAYER_RADIUS);
                let overlap = !col.footprint_clear(w[0], w[1], w[2], PLAYER_RADIUS, 8)
                    || !col.footprint_clear(w[0] + aim[0], w[1] + aim[1], w[2], PLAYER_RADIUS, 8);
                let kind = if ctrl_chest_blocked && planner_clear { n_h += 1; "HEIGHT #386" }
                    else if overlap { n_o += 1; "OVERLAP #381" }
                    else { n_x += 1; "OTHER" };
                println!("  [{kind:<12}] {zone}: wedged ({:.1},{:.1},{:.1}) start ({:.1},{:.1},{:.1}) goal ({:.1},{:.1},{:.1})",
                    w[0], w[1], w[2], s[0], s[1], s[2], g[0], g[1], g[2]);
            }
            println!("{zone:<12} {walked:>6} {wedged:>7} {n_h:>8} {n_o:>8} {n_x:>6}");
            tot_pairs += pairs.len(); tot_walked += walked; tot_wedged += wedged;
            tot_height += n_h; tot_overlap += n_o; tot_other += n_x;
        }
        let rate = if tot_walked > 0 { 100.0 * tot_wedged as f32 / tot_walked as f32 } else { 0.0 };
        println!("\n=== FAITHFUL WALKER DRIFT: {tot_walked} full journeys walked, {tot_wedged} terminal wedges \
            ({rate:.2}%) — height #386: {tot_height}, overlap #381: {tot_overlap}, other: {tot_other} ===");
        let _ = tot_pairs;
        assert!(tot_walked > 0, "no journeys walked — check $ZONE_DIR");
    }

    /// **D-2 SHAPE PROBE (temporary): is the qcat inverted-floor structurally distinguishable from the
    /// D-1 open-air-ceiling fixture?** Dumps every surface in the qcat wedge column facing-blind, with
    /// the distance UP to the next SOLID surface (headroom) and whether water sits above — to decide
    /// whether `is_standable` (facing-blind + headroom + anchoring) can accept -42.97 while rejecting a
    /// ceiling. Not a gate; a design probe.
    #[test]
    #[ignore = "requires qcat glb; D-2 shape probe"]
    fn probe_qcat_column_vs_fixture() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let mut col = Collision::build(&za, 32.0);
        col.set_water(crate::region_map::RegionMap::load(&std::path::Path::new(&dir).join("maps/water"), "qcat").map(std::sync::Arc::new));
        let (x, y) = (4.0f32, 809.8);
        println!("qcat column at ({x},{y}):  z_min={:.1} z_max={:.1}", col.z_min, col.z_max);
        // Enumerate surfaces facing-blind, top→down, via ground_below stepping.
        let mut top = col.z_max;
        for _ in 0..30 {
            let Some(s) = col.ground_below(x, y, top, col.z_max - col.z_min + 10.0) else { break };
            top = s - 0.5;
            // headroom UP: distance to next solid surface above `s`.
            let head = (1..400).map(|k| s + k as f32 * 0.5)
                .find(|&z| col.nearest_hit_t([x, y, z - 0.25], [x, y, z + 0.25]).is_some())
                .map(|z| z - s);
            let up_facing = col.nearest_floor(x, y, s, 0.6, 0.6).is_some(); // filtered → up-facing (or valve)
            let water_above = col.in_water([x, y, s + 1.0]) || col.in_water([x, y, s + 3.0]);
            println!("  surface z={s:8.2}  up_facing(filtered)={up_facing:5}  headroom_to_solid_above={head:?}  water_above={water_above}");
        }
        // The two D-2-relevant surfaces the gates care about:
        println!("controller ground_below@-42: {:?}", col.ground_below(x, y, -42.0, 200.0));
        println!("planner column_floors@-43:   {:?}", col.column_floors(x, y, -43.0, 20.0, 30.0));
    }

    // ─────────────────────── PR-D / D-1: support-axis drift (#375) fixtures ───────────────────────
    // These prove the support-axis bug and gate the fix. The RED-on-main test reproduces the LIVE qcat
    // wedge (the planner deletes the floor the controller stands on). The two #329 guards are GREEN on
    // main (the facing filter incidentally satisfies them) and MUST stay green through D-2, when the
    // facing-blind `is_standable` classifier must reject ceilings via headroom+anchoring instead.

    /// An UP-facing floor plane at height `z` over east [e0,e1] × north [-100,100] (`tri_nz > 0`, seen
    /// by `nearest_floor`). NB: the older `floor_band` helper's winding is actually *down*-facing — it
    /// is only ever used with facing-BLIND queries (`path_clear`/`line_clear`), so its facing never
    /// mattered; these PR-D fixtures DO care about facing, so they use these explicit helpers instead
    /// (both windings verified by `floor_and_ceiling_windings_are_as_labelled`).
    fn floor_up(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 2, 1, 0, 3, 2], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// A DOWN-facing ceiling plane at height `z` (`tri_nz < 0`, discarded by the facing filter).
    fn ceiling_down(z: f32, e0: f32, e1: f32) -> MeshData {
        MeshData {
            positions: vec![[-100.0, z, e0], [100.0, z, e0], [100.0, z, e1], [-100.0, z, e1]],
            normals: vec![[0.0, -1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        }
    }

    /// **RETRACTED at D-2: `open_air_ceiling_is_never_returned_as_floor`** (the owner-approved D-1
    /// fixture) and its winding-sanity companion. Both asserted that a down-facing surface with OPEN
    /// SKY above it (floor at z=0, ceiling at z=8, nothing on top) is a *ceiling* `nearest_floor` must
    /// never return.
    ///
    /// **That premise is FALSIFIED by qcat.** The D-2 shape probe (`probe_qcat_column_vs_fixture`,
    /// measured 2026-07-14) found the character's walkable −42.97 surface is DOWN-facing, with NOTHING
    /// solid above it, and an up-facing floor 13u below — geometrically *identical* to the fixture's
    /// z=8. So "down-facing + open above = ceiling" is wrong: qcat proves such a surface is walkable
    /// floor. A facing-blind classifier that rejected the fixture's z=8 would also delete qcat's
    /// walkway — the very bug #375 fixes. The owner reviewed this measurement and RETRACTED the fixture.
    ///
    /// The genuine #329 ceilings are caught by the two gates that replaced it:
    /// `close_roof_ceiling_is_rejected_by_headroom` (a ceiling with a roof close above → low headroom)
    /// and `qcat_pocket_nearest_floor_is_never_the_ceiling` (a far roof, excluded by the `ref_z`
    /// window). See the design doc's "RETRACTED" note.

    /// **THE #329 CLOSE-ROOF GATE (D-2, mutation-checked).** A realistic ceiling — a down-facing
    /// surface with a solid roof CLOSE above it — must be rejected by the headroom test
    /// (`headroom_to_next_solid_above < NAV_AGENT_HEIGHT`). This is a two-storey sandwich: room-A floor
    /// at z=0 (10u of headroom → standable), room-A ceiling at z=10 (down-facing, only 1u below
    /// room-B's floor → headroom 1 → REJECTED), room-B floor at z=11 (standable). The ceiling at z=10
    /// must NEVER be returned; both real floors (0 and 11) must be.
    ///
    /// This is NOT the #372 "decorative rock slab" cheat — there the slab was cosmetic while the
    /// classifier still used winding; here the roof-above IS the classifier's real input (a ceiling has
    /// a roof; that is what makes it a ceiling, not its winding). Mutation-check: drop the headroom test
    /// (see the commented line) → the ceiling at z=10 becomes "standable" → `nearest_floor@10` returns
    /// ~10 → RED.
    #[test]
    fn close_roof_ceiling_is_rejected_by_headroom() {
        let col = Collision::build(&ZoneAssets {
            terrain: vec![
                floor_up(0.0, -100.0, 100.0),      // room-A floor (10u headroom → standable)
                ceiling_down(10.0, -100.0, 100.0), // room-A ceiling (1u below room-B floor → rejected)
                floor_up(11.0, -100.0, 100.0),     // room-B floor (open above → standable)
            ], objects: vec![], textures: vec![],
        }, 32.0);
        // The standable set (column_floors) must contain the two real floors (0 and 11) but NOT the
        // low-headroom ceiling (10) — precise, unlike a nearest-to-ref_z check where 10 and 11 are
        // only 1u apart.
        let floors = col.column_floors(0.0, 0.0, 5.0, 20.0, 20.0);
        assert!(!floors.iter().any(|&z| (z - 10.0).abs() < 0.5),
            "column_floors contains the low-headroom ceiling z=10 (set {floors:?}) — the headroom test \
             failed to reject it (#329). MUTATION: drop the headroom check and this fires.");
        assert!(floors.iter().any(|&z| z.abs() < 0.5),
            "room-A floor z=0 (10u headroom) must be standable (set {floors:?})");
        assert!(floors.iter().any(|&z| (z - 11.0).abs() < 0.5),
            "room-B floor z=11 (open above) must be standable (set {floors:?})");
    }

    /// **D-2 winding sanity (facing-blind now):** after D-2 `nearest_floor` is FACING-BLIND, so it
    /// accepts an inverted (down-facing) floor with clearance above — that is the whole #375 fix. This
    /// pins that: a lone `floor_up` is standable AND a lone `ceiling_down` with open air above is ALSO
    /// standable now (it is walkable floor, per qcat). Contrast `close_roof_ceiling_*`: only a ceiling
    /// with a *roof close above* is rejected.
    #[test]
    fn nearest_floor_is_facing_blind_after_d2() {
        let up = Collision::build(&ZoneAssets {
            terrain: vec![floor_up(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 32.0);
        assert!(up.nearest_floor(0.0, 0.0, 0.0, 5.0, 20.0).is_some_and(|z| z.abs() < 0.5),
            "an up-facing floor is standable");
        let down = Collision::build(&ZoneAssets {
            terrain: vec![ceiling_down(0.0, -100.0, 100.0)], objects: vec![], textures: vec![] }, 32.0);
        assert!(down.nearest_floor(0.0, 0.0, 0.0, 5.0, 20.0).is_some_and(|z| z.abs() < 0.5),
            "a down-facing surface with open air above is ALSO standable now (facing-blind) — the qcat fix");
    }

    /// **THE #329 QCAT-POCKET GATE (D-1, asset; GREEN on main).** At the qcat spawn pocket the column
    /// is `[roof 391.8, floor -70.0]` (#329, `assets.rs` column comment). The planner must never treat
    /// the 391.8 roof as ground. GREEN on `main` (facing filter); D-2's facing-blind classifier must
    /// keep it green via headroom+anchoring (the roof has rock above it → fails headroom).
    #[test]
    #[ignore = "requires the cached qcat glb at $ZONE_DIR; #329 guard — GREEN on main, stays green through D-2"]
    fn qcat_pocket_nearest_floor_is_never_the_ceiling() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let col = Collision::build(&za, 32.0);
        // The #329 spawn pocket XY. The floor is ~-70; the catacombs roof is ~+391.8.
        let f = col.nearest_floor(-48.0, 1058.0, -66.0, 20.0, 100.0);
        assert!(f.is_some_and(|z| z < 100.0),
            "qcat pocket nearest_floor returned the ceiling (got {f:?}) — #329 reintroduced");
        // And the whole column's floor set must contain no ceiling-height surface.
        let floors = col.column_floors(-48.0, 1058.0, -66.0, 20.0, 500.0);
        assert!(floors.iter().all(|&z| z < 100.0),
            "qcat pocket column_floors contains a ceiling-height surface: {floors:?} — #329");
    }

    /// **THE SUPPORT-AXIS DRIFT — RED ON MAIN (#375).** The live wedge captured 2026-07-14: a plain
    /// `zone_cross` qcat→qeynos2 wedged TERMINALLY at `(4.0, 809.8, -43.0)` with a full route in hand.
    /// Root cause, pinned here: the controller's ground model (`ground_below`, facing-blind) stands the
    /// character on solid floor at **z ≈ -42.97**, while the planner's floor model (`column_floors`,
    /// up-facing-only) sees **only z ≈ -55.97** there — the -42.97 walkway is inverted (down-facing)
    /// art the facing filter deletes. The two disagree about the floor, so the planner routes as if the
    /// character were elsewhere (or floating) and loops.
    ///
    /// This asserts the invariant PR-D restores: **the planner sees the floor the controller stands
    /// on.** It was RED on `main` (the planner's set omitted -42.97); **it is GREEN at D-2** — both
    /// sides now share `is_standable`, so `column_floors` includes the inverted-art walkway. It is the
    /// falsifiable, deterministic proof of the fix, independent of any live run.
    ///
    /// **CI note:** the coordinator asked to "un-ignore" this so it runs live. It is asset-gated (needs
    /// the qcat glb, absent on the CI runner — #357), and `from_glb().unwrap()` would panic there, so
    /// it stays `#[ignore]`d like every other baked-asset test. It is verified GREEN locally at D-2
    /// (`ZONE_DIR=… cargo test --release --lib qcat_support_floor_is_visible -- --ignored`). Literal
    /// un-ignoring is not possible without bundling the asset into CI; flagged in the PR.
    #[test]
    #[ignore = "requires the cached qcat glb at $ZONE_DIR (#357); GREEN at D-2 — proves the support-axis FIX (#375)"]
    fn qcat_support_floor_is_visible_to_the_planner() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let za = ZoneAssets::from_glb(&std::path::Path::new(&dir).join("qcat.glb")).unwrap();
        let col = Collision::build(&za, 32.0);
        let (x, y) = (4.0f32, 809.8);
        // The controller's ground clamp (facing-blind) — what the walker actually stands on.
        let ground = col.ground_below(x, y, -42.0, 200.0)
            .expect("the controller's ground_below must find the walkway the walker stood on");
        assert!((ground - (-42.97)).abs() < 1.5,
            "sanity: the controller ground at the wedge XY should be ~-42.97, got {ground:.2}");
        // The planner's floor set (up-facing filter) — what A* plans over.
        let planner_floors = col.column_floors(x, y, -43.0, 20.0, 30.0);
        assert!(planner_floors.iter().any(|&f| (f - ground).abs() < 1.5),
            "SUPPORT-AXIS DRIFT (#375): the planner's floor set {planner_floors:?} does NOT include the \
             floor the controller stands on ({ground:.2}). The facing filter deleted the inverted-art \
             walkway — the planner and walker disagree about where the floor is, which is the live qcat \
             terminal wedge. RED on main; GREEN after the shared is_standable predicate (D-2).");
    }


    /// **§C REVIEW CONTRACT (D-2, mutation-relevant): the destination is judged on its OWN column,
    /// never vetoed for being far from the source `ref_z`.** A large DROP's landing floor must be
    /// standable when the planner probes it from the SOURCE cell's height — otherwise the controlled-
    /// fall edge vanishes and nav can descend into a level it cannot leave. `is_standable` is a property
    /// of the destination surface's own column (flatness + headroom to the next solid above it); `ref_z`
    /// only *windows* which surfaces are in range, it does NOT gate standability. If a future edit made
    /// standability depend on `|surface_z − ref_z|` (the tight-anchoring mistake §C warns against — it
    /// also seals ramps, which A* climbs ~9.6u/cell at MAX_WALK_GRADE), this fixture fires.
    #[test]
    fn is_standable_judges_the_destination_on_its_own_column_not_the_source_z() {
        // High floor over east[-100,0] at z=0; low floor over east[0,100] at z=-40 — a 40u drop at east=0.
        let col = Collision::build(&ZoneAssets {
            terrain: vec![floor_up(0.0, -100.0, 0.0), floor_up(-40.0, 0.0, 100.0)],
            objects: vec![], textures: vec![],
        }, 32.0);
        // Probed from the SOURCE (high) floor's z=0 with a drop-sized `down` window: the landing floor
        // 40u BELOW ref_z must be standable. A tight `|surface_z − ref_z|` gate would wrongly veto it.
        let from_source = col.column_floors(20.0, 0.0, 0.0, 5.0, 60.0);
        assert!(from_source.iter().any(|&z| (z - (-40.0)).abs() < 0.5),
            "§C: the drop landing (z=-40) must be standable when probed from the SOURCE z=0 (set \
             {from_source:?}) — is_standable must judge the destination on its own column, not veto it \
             for distance from ref_z (that would delete the fall edge AND seal ramps).");
        // And the controlled-fall edge actually forms: A* routes the high floor → low floor.
        let path = col.find_path([-20.0, 0.0, 0.0], [20.0, 0.0, -40.0], crate::movement::PLAYER_RADIUS, &[], true);
        assert!(path.is_some(), "A* must route the 40u drop (high floor → low floor); the fall edge must survive D-2");
    }

    /// **Q1 SEAL MEASUREMENT (#375, D-2 crux).** The owner's Q1: does the anchoring-first `headroom`
    /// re-delete *legitimately-standable inverted ledges*? For each inverted-art zone, sample surfaces
    /// facing-blind (the physical truth of what geometry exists), keep the ones a body fits on
    /// (`footprint_clear`), and split them by what `is_standable` decides:
    ///   * **RECOVERED** — `is_standable` accepts it (a floor the old facing filter would have deleted
    ///     for being down-facing, now correctly kept). This is the #375 win.
    ///   * **HEADROOM-REJECT** — `is_standable` rejects it purely because `headroom < NAV_AGENT_HEIGHT`
    ///     (a roof close above). The Q1 question is whether these are real ceilings (correct) or legit
    ///     low ledges (a seal). Cross-checked against route-success (≥ 99.50%): if these were real
    ///     floors, routes over them would fail — and route-success did NOT drop.
    ///   * **STEEP-REJECT** — rejected for `|nz| < NAV_NEAR_HORIZONTAL` (a wall/steep slope A*'s grade
    ///     limit rejects anyway).
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test --release --lib q1_headroom_seal_measurement -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; the Q1 seal measurement (#375)"]
    fn q1_headroom_seal_measurement() {
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| ["highpass", "permafrost", "neriakc", "qcat"].iter().map(|s| s.to_string()).collect());
        let mut seed: u64 = 0x0155_EA1D;
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };
        println!("\n{:<12} {:>10} {:>10} {:>12} {:>10}", "zone", "fits", "recovered", "headroom-rej", "steep-rej");
        for zone in &zones {
            let Ok(za) = ZoneAssets::from_glb(&std::path::Path::new(&dir).join(format!("{zone}.glb"))) else {
                println!("{zone:<12}  (no glb)"); continue };
            let col = Collision::build(&za, 32.0);
            if col.cols == 0 { continue; }
            let (ext_e, ext_n) = (col.cols as f32 * col.cell_size, col.rows as f32 * col.cell_size);
            let (mut fits, mut recovered, mut head_rej, mut steep_rej) = (0usize, 0usize, 0usize, 0usize);
            let mut cols = 0;
            while cols < 12000 && fits < 8000 {
                cols += 1;
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * ext_e;
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * ext_n;
                // Every surface in the column, facing-blind (physical geometry).
                let surfs = col.column_surfaces(e, n, 0.5 * (col.z_min + col.z_max),
                    0.5 * (col.z_max - col.z_min) + 1.0, 0.5 * (col.z_max - col.z_min) + 1.0);
                for &(z, nz) in &surfs {
                    if !col.footprint_clear(e, n, z, crate::movement::PLAYER_RADIUS, 8) { continue; }
                    fits += 1;
                    // Recompute is_standable's verdict + reason for this surface.
                    if nz.abs() < NAV_NEAR_HORIZONTAL { steep_rej += 1; continue; }
                    // headroom to next SOLID above (facing-blind).
                    let mut head = f32::INFINITY;
                    for &(zz, _) in &surfs { if zz > z + 0.3 { head = head.min(zz - z); } }
                    if head < NAV_AGENT_HEIGHT { head_rej += 1; } else { recovered += 1; }
                }
            }
            println!("{zone:<12} {fits:>10} {recovered:>10} {head_rej:>12} {steep_rej:>10}");
        }
        println!("\n=== Q1: 'recovered' = inverted/any floor is_standable KEEPS (the #375 win). \
            'headroom-rej' = rejected for a roof < {NAV_AGENT_HEIGHT}u above (real ceilings — cross-checked \
            by route-success ≥ 99.50%, which did NOT drop, so these are not legit floors being sealed). ===");
    }

    /// **THE FLOOR-MODEL DISAGREEMENT SCAN — a corpus indicator for D-2 (#375).** Counts, over a zone
    /// corpus, points where the controller's floor model (`ground_below`, facing-blind) finds a
    /// standable-looking surface the planner's floor model (`column_floors`, facing-filtered) omits.
    /// That disagreement is the support-axis drift; the live qcat wedge is one instance of it.
    ///
    /// **HONEST LIMIT (read before trusting the number).** On `main` this is an **UPPER BOUND**, not a
    /// clean drift count. A simple facing-blind + footprint + headroom filter **cannot distinguish** a
    /// real inverted-art FLOOR the character stands on (qcat's −42.97 walkway) from a genuine
    /// ceiling/overhang UNDERSIDE that merely happens to have open space above it — and telling those
    /// two apart is *exactly what PR-D's `is_standable` (headroom AND anchoring) adds*. So on `main`
    /// this over-counts in ceiling-rich zones (a city like qeynos2 reads high not because half its
    /// floor is inverted, but because it has many undersides). The one zone that is a believable clean
    /// control is a big OPEN-terrain outdoor zone with few ceilings (**gfaydark ≈ 0.2%**). Treat the
    /// per-zone numbers as "how much does the floor model disagree here", not "how many real bugs."
    ///
    /// **Why this is still the right corpus signal for D-2.** After D-2 the planner and the controller
    /// call the SAME `is_standable`, so this disagreement is **0 by construction, in every zone** — a
    /// blunt but genuine regression gate (any nonzero after D-2 means the two sides did not actually
    /// unify). The SHARP gate — that D-2 makes the planner see qcat's real floor WITHOUT admitting
    /// ceilings — is the focused pair below (`qcat_support_floor_is_visible_to_the_planner`, RED→GREEN;
    /// `open_air_ceiling_*` / `qcat_pocket_*`, stay GREEN). This scan is the breadth check; those are
    /// the correctness gate.
    ///
    /// (Why not a swim-simulation scanner: the drift is a STATIC floor-model property — the two floor
    /// queries disagree whether or not anyone is swimming — so measuring it statically is deterministic
    /// and needs no buoyancy fidelity. A swim-capable dynamic scanner would only re-derive, less
    /// reliably, what this and the focused qcat fixture already pin.)
    ///
    /// ```text
    /// ZONE_DIR=~/.local/share/eqoxide/assets/models \
    ///   cargo test --release --lib floor_model_disagreement_scan -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "requires baked zone glbs at $ZONE_DIR; the D-2 floor-model-disagreement breadth signal (#375)"]
    fn floor_model_disagreement_scan() {
        const TOL: f32 = 1.5;      // a planner floor within this of the controller's ground = agreement
        const HEADROOM: f32 = 5.0;  // open space (to next SOLID surface) a standable spot needs above it
        const RADIUS: f32 = crate::movement::PLAYER_RADIUS;
        let dir = std::env::var("ZONE_DIR")
            .unwrap_or_else(|_| format!("{}/.local/share/eqoxide/assets/models", std::env::var("HOME").unwrap()));
        // The inverted-art zones #375 names, plus a few normal zones as a clean-art control (their
        // count should already be ~0 on main, and must stay 0 after D-2 — a regression guard).
        let zones: Vec<String> = std::env::var("ZONES").ok()
            .map(|z| z.split(',').map(str::to_string).collect())
            .unwrap_or_else(|| vec![
                "qcat", "highpass", "permafrost", "neriakc",  // inverted-art zones (#375) — high on main
                "gfaydark",                                   // open outdoor: the believable clean control (~0)
                "qeynos2",                                    // ceiling-rich city: reads high (see HONEST LIMIT)
            ].into_iter().map(str::to_string).collect());

        let mut seed: u64 = 0x5044_0F7D; // distinct stream
        let mut rnd = || { seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); (seed >> 33) as u32 };

        let mut tot_pts = 0usize;
        let mut tot_drift = 0usize;
        println!("\n{:<12} {:>10} {:>12} {:>8}", "zone", "sampled", "drift-pts", "%");
        for zone in &zones {
            let p = std::path::Path::new(&dir).join(format!("{zone}.glb"));
            let Ok(za) = ZoneAssets::from_glb(&p) else { println!("{zone:<12}  (no glb — skipped)"); continue };
            let col = Collision::build(&za, 32.0);
            if col.cols == 0 { println!("{zone:<12}  (no grid — skipped)"); continue; }
            let ext_e = col.cols as f32 * col.cell_size;
            let ext_n = col.rows as f32 * col.cell_size;
            let (mut sampled, mut drift) = (0usize, 0usize);
            let mut cols_probed = 0;
            let zreach = (col.z_max - col.z_min).max(1.0) + 10.0;
            // Enumerate every surface in a sampled column FACING-BLIND, by walking `ground_below` down
            // from the top — no planner model, no z-sampling artifact (the invisible-boundary art at
            // z≈−32768 that made uniform-z sampling miss gfaydark/akanon is simply never a standable
            // surface, so it is filtered out below rather than dominating the sample space).
            while cols_probed < 20000 && sampled < 6000 {
                cols_probed += 1;
                let e = col.origin[0] + (rnd() as f32 / u32::MAX as f32) * ext_e;
                let n = col.origin[1] + (rnd() as f32 / u32::MAX as f32) * ext_n;
                let mut top = col.z_max;
                for _ in 0..24 { // at most 24 surfaces per column
                    let Some(ground) = col.ground_below(e, n, top, zreach) else { break };
                    top = ground - 0.5; // next probe starts just below this surface
                    // Controller-standable filter: body fits + HEADROOM of open air above (to next SOLID
                    // surface). Excludes ceilings/roof-undersides — a character cannot stand there.
                    if !col.footprint_clear(e, n, ground, RADIUS, 8) { continue; }
                    if col.nearest_hit_t([e, n, ground + 0.5], [e, n, ground + HEADROOM]).is_some() { continue; }
                    sampled += 1;
                    let floors = col.column_floors(e, n, ground, 20.0, 20.0);
                    if !floors.iter().any(|&f| (f - ground).abs() <= TOL) { drift += 1; }
                }
            }
            let pct = if sampled > 0 { 100.0 * drift as f32 / sampled as f32 } else { 0.0 };
            println!("{zone:<12} {sampled:>10} {drift:>12} {pct:>7.2}%", );
            tot_pts += sampled; tot_drift += drift;
        }
        println!("\n=== FLOOR-MODEL DISAGREEMENT: {tot_drift} / {tot_pts} standable-looking surfaces the \
            planner's floor model omits (UPPER BOUND — conflates inverted-art floor with ceilings on main; \
            see HONEST LIMIT). D-2 gate: → 0 by construction (both sides share is_standable). ===");
        assert!(tot_pts > 0, "no points sampled — check $ZONE_DIR");
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

    /// #259: a sunken, water-filled pit in the middle of an otherwise-open street. The pit floor
    /// is a legal one-way DROP from the rim (MAX_STEP_DOWN is generous) but climbing back out is
    /// capped at `WATER_EXIT_UP` (~2.5u above the water surface) — the water surface here sits
    /// 10u below the rim, so once in, there is no walkable way out. With the search radius bounded
    /// (mirroring the real `plan_path` fallback's local-tier cap once every full-route radius has
    /// failed), the far-side goal and any walk-around are both out of reach, forcing a genuine
    /// PARTIAL route — exactly the condition under which `best_toward` used to land inside the pit
    /// (closer by straight-line heuristic than any street cell) and drive the walker into the trap.
    #[test]
    fn find_path_does_not_drive_a_partial_route_into_a_sunken_water_pit() {
        let quad = |v: Vec<[f32; 3]>| MeshData {
            positions: v, normals: vec![], uvs: vec![], indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        // MeshData pos = [north, up, east]; Collision maps to world [east, north, up].
        // A street "frame" (four strips) around a 40x40 hole (east 80..120, north 80..120).
        let south = quad(vec![[0.0, 0.0, 0.0],   [0.0, 0.0, 200.0],   [80.0, 0.0, 200.0],   [80.0, 0.0, 0.0]]);
        let north = quad(vec![[120.0, 0.0, 0.0], [120.0, 0.0, 200.0], [200.0, 0.0, 200.0],  [200.0, 0.0, 0.0]]);
        let west  = quad(vec![[80.0, 0.0, 0.0],  [80.0, 0.0, 80.0],   [120.0, 0.0, 80.0],   [120.0, 0.0, 0.0]]);
        let east  = quad(vec![[80.0, 0.0, 120.0],[80.0, 0.0, 200.0],  [120.0, 0.0, 200.0],  [120.0, 0.0, 120.0]]);
        // Pit floor, 20u below the street, extending slightly under the frame's hole edges so the
        // rim cells find it as a column drop (not disconnected geometry).
        let pit = quad(vec![[70.0, -40.0, 70.0], [70.0, -40.0, 130.0], [130.0, -40.0, 130.0], [130.0, -40.0, 70.0]]);
        let assets = ZoneAssets { terrain: vec![south, north, west, east, pit], objects: vec![], textures: vec![] };

        let mut col = Collision::build(&assets, 8.0);
        // Water fills only the pit's own footprint, up to 30u below street level (z=-30) — far too
        // deep for either the dry grade limit or WATER_EXIT_UP to reach the z=0 rim from the
        // surface. Bounded to the pit's XY box (unlike `flat_below`, which is a global z-split) so
        // the "swim across the surface" edge can't misfire against the dry street elsewhere.
        col.set_water(Some(std::sync::Arc::new(
            crate::region_map::RegionMap::box_below(70.0, 130.0, 70.0, 130.0, -30.0))));

        // Sanity: the pit is reachable (a legal drop) but NOT climbable back out — a genuine
        // one-way trap, the real structural bug independent of the fix under test.
        let into_pit = col.find_path([60.0, 100.0, 0.0], [100.0, 100.0, -40.0], 1.0, &[], false);
        assert!(into_pit.is_some(), "the pit floor must be a legal drop from the rim");
        let out_of_pit = col.find_path([100.0, 100.0, -40.0], [60.0, 100.0, 0.0], 1.0, &[], false);
        assert!(out_of_pit.is_none(), "the pit must have no walkable exit — a one-way trap");

        // A full route DOES exist by going around the hole (north or south corridor) — confirms
        // this is a solvable street layout, not a sealed level.
        let start = [60.0f32, 100.0, 0.0];
        let goal  = [140.0f32, 100.0, 0.0];
        assert!(col.find_path(start, goal, 1.0, &[], false).is_some(),
            "a full route around the pit must exist when the search isn't radius-bounded");

        // Radius-bound the search (as the real fallback does) so neither the goal nor the
        // walk-around is reachable — forcing a genuine PARTIAL route toward the goal.
        let path = col.find_path_res(start, goal, 1.0, &[], true, 8.0, Some(50.0), 0.0, PlanCtx::default())
            .expect("a partial route toward the goal should still make progress");
        let last = *path.last().unwrap();
        let last_wet = col.in_water(last);
        eprintln!("partial route: {} waypoints, last={last:?} in_water={last_wet}", path.len());
        assert!(!last_wet, "#259: a partial route must never end submerged in the pit — that \
            strands the walker in a one-way trap. last={last:?}");
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
