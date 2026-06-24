//! Character model loading from glTF/GLB: meshes + per-vertex skin weights, textures, the skeleton
//! and animation clips, per-clip posed bounds (used to recenter + ground a model on its current
//! pose rather than its bind pose), and archetype scale. See `docs/character-models.md`.

use anyhow::{Context, Result};
use std::path::Path;
use crate::assets::{MeshData, TextureData};
use crate::anim::{AnimClip, GroundProbe, JointChannel, JointProperty, SkinData};

/// Per-vertex joint skinning data for one mesh primitive (parallel to MeshData positions).
pub struct SkinnedMeshData {
    pub joint_indices: Vec<[u32; 4]>,
    pub joint_weights: Vec<[f32; 4]>,
}

pub struct ModelAsset {
    pub meshes:            Vec<MeshData>,
    pub textures:          Vec<TextureData>,
    pub skin:              Option<SkinData>,
    pub skin_meshes:       Vec<Option<SkinnedMeshData>>,  // parallel to meshes
    /// Dominant node_scale for the model (maximum across all mesh nodes).
    /// 1.0 for static; 100.0 for Quaternius/CC0 skinned models.
    pub skinned_node_scale: f32,
    /// Per-mesh node_scale, parallel to meshes. Accessory meshes (weapon, backpack) often
    /// have a different scale than the body mesh; the render pass applies each independently.
    pub skinned_mesh_scales: Vec<f32>,
    /// Distance from Y=0 to the model bottom in buffer vertex space computed from the dominant-
    /// scale meshes only. For static models node_scale is baked in; for skinned models these are
    /// raw pre-node-scale positions.  Lift = y_bottom × mesh_scale (dominant).
    pub y_bottom:          f32,
    /// Vertical extent of the model (max_y - min_y) in buffer vertex space.
    /// Used for visual_scale: visual_scale = 2 × y_extent × arch_scale.
    pub y_extent:          f32,
    /// Center of the model in the X and Z axes (raw pre-node-scale space, dominant meshes only).
    /// Used as a centering correction so models are rendered at their entity position rather than
    /// offset by the model's origin-to-center distance.
    pub x_center:          f32,
    pub z_center:          f32,
    /// Lowercase race+gender prefix from material names (e.g. "hom"). Empty if unknown.
    pub prefix: String,
    /// Per-mesh equipment slot binding, parallel to `meshes`. `None` = not an armor slot.
    pub equip_slots: Vec<Option<EquipSlot>>,
    /// True model height in EQ units, from the `eq_height` extras field written by the
    /// converter into the glTF ROOT node. Falls back to `y_extent` (measured vertex bounds)
    /// when the extras field is absent (e.g. chr.s3d static models).
    pub true_height: f32,
    /// Per-animation-clip posed bounds: (center_x [p0], center_z [p2], feet_floor [min p1]),
    /// parallel to `skin.clips`. Used to recenter + ground from the CURRENT clip instead of
    /// the bind pose (the live animation pose differs from bind, causing a static offset).
    /// Empty for static/non-skinned models.
    pub clip_bounds: Vec<(f32, f32, f32)>,
    /// Robust "feet" height (model-space Y, idle pose): the 5th percentile of the posed
    /// vertices' Y, which excludes stray geometry that hangs below the visible feet. The
    /// renderer grounds a skinned model by lifting `-feet_offset × mesh_scale`. Per-model
    /// so every archetype grounds by its own feet (not a humanoid-tuned constant). 0 if no skin.
    pub feet_offset: f32,
}

impl ModelAsset {
    pub fn load(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open glTF: {}", path.display()))?;
        let gltf_doc = gltf::Gltf::from_reader(std::io::BufReader::new(file))
            .with_context(|| format!("failed to parse glTF: {}", path.display()))?;
        let base = path.parent().unwrap_or_else(|| std::path::Path::new("./"));
        let buffers = gltf::import_buffers(&gltf_doc.document, Some(base), gltf_doc.blob)
            .with_context(|| format!("failed to load glTF buffers: {}", path.display()))?;
        let raw_images = gltf::import_images(&gltf_doc.document, Some(base), &buffers);
        if let Err(ref e) = raw_images {
            tracing::warn!("models: import_images failed for {}: {}", path.display(), e);
        }
        let images: Vec<gltf::image::Data> = raw_images.unwrap_or_default();

        let mut textures: Vec<TextureData> = Vec::new();
        for (i, image) in images.iter().enumerate() {
            let rgba = match image.format {
                gltf::image::Format::R8G8B8A8 => image.pixels.clone(),
                gltf::image::Format::R8G8B8 => image.pixels
                    .chunks(3)
                    .flat_map(|rgb| [rgb[0], rgb[1], rgb[2], 255u8])
                    .collect(),
                _ => {
                    tracing::info!("models: skipping image {} with unsupported format", i);
                    continue;
                }
            };
            textures.push(TextureData {
                name: i.to_string(), width: image.width, height: image.height, rgba,
            });
        }

        let document = &gltf_doc.document;

        // ── Read eq_height from the first node that carries it in extras ──────
        // The converter writes this field into the ROOT node's extras so the loader
        // can recover the true EQ-unit height without measuring raw vertex bounds.
        let eq_height_from_extras: f32 = document.nodes()
            .find_map(|n| {
                let ex = n.extras().as_ref()?;
                let v: serde_json::Value = serde_json::from_str(ex.get()).ok()?;
                v.get("eq_height").and_then(|h| h.as_f64()).map(|h| h as f32)
            })
            .filter(|h| *h > 0.0)
            .unwrap_or(0.0); // 0.0 = "use measured extent" sentinel; finalized below

        // ── Skin: joint hierarchy + inverse bind matrices ─────────────────────
        let skin_opt = document.skins().next();
        let (mut skin_data, _joint_index_map) = if let Some(skin) = skin_opt {
            let joints: Vec<usize> = skin.joints().map(|n| n.index()).collect();
            let joint_count = joints.len();

            // Map node index → joint array index
            let joint_index_map: std::collections::HashMap<usize, usize> =
                joints.iter().enumerate().map(|(i, &n)| (n, i)).collect();

            // Build parent array: parent[j] = index of j's parent joint (if any)
            let mut parents: Vec<Option<usize>> = vec![None; joint_count];
            for node in document.nodes() {
                for child in node.children() {
                    if let (Some(&pi), Some(&ci)) =
                        (joint_index_map.get(&node.index()), joint_index_map.get(&child.index()))
                    {
                        parents[ci] = Some(pi);
                    }
                }
            }

            // Inverse bind matrices
            let skin_reader = skin.reader(|buf| Some(&buffers[buf.index()]));
            let id4 = [[1.0f32,0.0,0.0,0.0],[0.0,1.0,0.0,0.0],[0.0,0.0,1.0,0.0],[0.0,0.0,0.0,1.0]];
            let inv_bind: Vec<[[f32; 4]; 4]> = skin_reader
                .read_inverse_bind_matrices()
                .map(|iter| iter.collect())
                .unwrap_or_else(|| vec![id4; joint_count]);

            // Rest pose: collect each joint's local transform at bind time. Used
            // as the initial value in evaluate() for joints that have no channel
            // in a given clip (standard glTF exporters omit constant channels).
            let mut rest_translations = vec![[0.0f32; 3]; joint_count];
            let mut rest_rotations    = vec![[0.0f32, 0.0, 0.0, 1.0]; joint_count];
            let mut rest_scales       = vec![[1.0f32; 3]; joint_count];
            for node in document.nodes() {
                if let Some(&ji) = joint_index_map.get(&node.index()) {
                    let (t, r, s) = node.transform().decomposed();
                    rest_translations[ji] = t;
                    rest_rotations[ji]    = r;
                    rest_scales[ji]       = s;
                }
            }

            // ── Animation clips ───────────────────────────────────────────────
            let mut clips: Vec<AnimClip> = Vec::new();
            for anim in document.animations() {
                let mut channels: Vec<JointChannel> = Vec::new();
                let mut duration = 0.0f32;

                for ch in anim.channels() {
                    let node_idx = ch.target().node().index();
                    let joint_idx = match joint_index_map.get(&node_idx) {
                        Some(&j) => j,
                        None => continue,
                    };

                    let property = match ch.target().property() {
                        gltf::animation::Property::Translation => JointProperty::Translation,
                        gltf::animation::Property::Rotation    => JointProperty::Rotation,
                        gltf::animation::Property::Scale       => JointProperty::Scale,
                        gltf::animation::Property::MorphTargetWeights => continue,
                    };

                    let reader = ch.reader(|buf| Some(&buffers[buf.index()]));
                    let times: Vec<f32> = match reader.read_inputs() {
                        Some(it) => it.collect(),
                        None => continue,
                    };
                    if times.is_empty() { continue; }
                    if let Some(&t) = times.last() { duration = duration.max(t); }

                    let values: Vec<[f32; 4]> = match reader.read_outputs() {
                        Some(gltf::animation::util::ReadOutputs::Translations(it)) =>
                            it.map(|[x,y,z]| [x, y, z, 0.0]).collect(),
                        Some(gltf::animation::util::ReadOutputs::Rotations(it)) =>
                            it.into_f32().collect(),
                        Some(gltf::animation::util::ReadOutputs::Scales(it)) =>
                            it.map(|[x,y,z]| [x, y, z, 0.0]).collect(),
                        _ => continue,
                    };

                    channels.push(JointChannel { joint: joint_idx, property, times, values });
                }

                clips.push(AnimClip {
                    name:     anim.name().unwrap_or("").to_string(),
                    duration,
                    channels,
                });
            }

            let sd = SkinData { joint_count, parents, inv_bind, clips,
                                rest_translations, rest_rotations, rest_scales,
                                ground_probes: Vec::new() };
            (Some(sd), joint_index_map)
        } else {
            (None, std::collections::HashMap::new())
        };

        let is_skinned = skin_data.is_some();

        // ── Node scale per mesh ───────────────────────────────────────────────
        // For static models: bake node_scale into vertex positions.
        // For skinned models: store per-mesh node_scale separately (baking would corrupt joint
        // matrices). Models may have accessory meshes (weapons, backpacks) at a different
        // node_scale than the body — track each independently.
        let mut static_node_scale: std::collections::HashMap<usize, [f32; 3]> =
            std::collections::HashMap::new();
        let mut skinned_per_mesh_scale: std::collections::HashMap<usize, f32> =
            std::collections::HashMap::new();
        for node in document.nodes() {
            if let Some(m) = node.mesh() {
                let (_, _, s) = node.transform().decomposed();
                if is_skinned {
                    // s[0..2] should be equal (uniform); take x.
                    skinned_per_mesh_scale.insert(m.index(), s[0]);
                } else {
                    static_node_scale.insert(m.index(), s);
                }
            }
        }

        // Dominant scale = maximum per-mesh scale (the body mesh; accessories are smaller).
        let skinned_node_scale: f32 = skinned_per_mesh_scale.values()
            .cloned()
            .fold(1.0f32, f32::max);

        // ── Mesh primitives ───────────────────────────────────────────────────
        let mut meshes:             Vec<MeshData>               = Vec::new();
        let mut skin_meshes:        Vec<Option<SkinnedMeshData>> = Vec::new();
        let mut skinned_mesh_scales: Vec<f32>                   = Vec::new();
        let mut equip_slots: Vec<Option<EquipSlot>> = Vec::new();
        let mut model_prefix: String = String::new();

        for mesh in document.meshes() {
            let this_mesh_scale = if is_skinned {
                skinned_per_mesh_scale.get(&mesh.index()).copied().unwrap_or(1.0)
            } else {
                1.0 // static: already baked, scale is 1 at render time
            };
            // Skip accessory meshes (weapons, backpacks) authored at a different node_scale
            // with their own separate skin. These have incompatible inv_bind matrices and
            // cannot be skinned correctly by the shared skeleton without per-mesh skin loading.
            if is_skinned && (this_mesh_scale - skinned_node_scale).abs() > skinned_node_scale * 0.1 {
                continue;
            }
            let ns = if is_skinned {
                [1.0f32, 1.0, 1.0]  // vertices stay in raw (pre-node-scale) space
            } else {
                static_node_scale.get(&mesh.index()).copied().unwrap_or([1.0, 1.0, 1.0])
            };

            for primitive in mesh.primitives() {
                let reader = primitive.reader(|buf| Some(&buffers[buf.index()]));

                let positions: Vec<[f32; 3]> = match reader.read_positions() {
                    Some(p) => p.map(|[x,y,z]| [x*ns[0], y*ns[1], z*ns[2]]).collect(),
                    None => continue,
                };
                if positions.is_empty() { continue; }

                let normals: Vec<[f32; 3]> = reader.read_normals()
                    .map(|n| n.collect())
                    .unwrap_or_else(|| vec![[0.0, 0.0, 1.0]; positions.len()]);

                let uvs: Vec<[f32; 2]> = reader.read_tex_coords(0)
                    .map(|tc| tc.into_f32().collect())
                    .unwrap_or_else(|| vec![[0.0, 0.0]; positions.len()]);

                let indices: Vec<u32> = match reader.read_indices() {
                    Some(idx) => idx.into_u32().collect(),
                    None => continue,
                };

                let pbr = primitive.material().pbr_metallic_roughness();
                let texture_name = pbr.base_color_texture()
                    .map(|t| t.texture().source().index().to_string());
                let bc = pbr.base_color_factor();
                let base_color = [bc[0], bc[1], bc[2], bc[3]];

                // Skinning data (only when model has a skin)
                let sd_opt = if is_skinned {
                    let n = positions.len();
                    let joint_indices: Vec<[u32; 4]> = reader.read_joints(0)
                        .map(|j| j.into_u16()
                            .map(|[a,b,c,d]| [a as u32, b as u32, c as u32, d as u32])
                            .collect())
                        .unwrap_or_else(|| vec![[0u32; 4]; n]);
                    let joint_weights: Vec<[f32; 4]> = reader.read_weights(0)
                        .map(|w| w.into_f32().collect())
                        .unwrap_or_else(|| vec![[1.0, 0.0, 0.0, 0.0]; n]);
                    Some(SkinnedMeshData { joint_indices, joint_weights })
                } else {
                    None
                };

                meshes.push(MeshData {
                    positions, normals, uvs, indices, texture_name, base_color,
                    center: [0.0, 0.0, 0.0],
                });
                skin_meshes.push(sd_opt);
                skinned_mesh_scales.push(this_mesh_scale);
                let parsed = primitive.material().name().and_then(parse_equip_material);
                if model_prefix.is_empty() {
                    if let Some((ref p, _)) = parsed { model_prefix = p.clone(); }
                }
                equip_slots.push(parsed.map(|(_, s)| s));
            }
        }

        anyhow::ensure!(!meshes.is_empty(), "glTF file has no renderable primitives");

        // ── Ground probes ─────────────────────────────────────────────────────
        // Skin the bind pose, find the lowest-Z vertices in render space (the feet),
        // and keep them so the render passes can ground the model by its actual posed
        // lowest point. This is correct even for rigs that reorient the raw mesh (the
        // Skeleton), where raw-mesh y_bottom does not match the skinned height.
        if let Some(sd) = skin_data.as_mut() {
            // Collect every skinned vertex of the dominant body meshes as a candidate.
            // We sample broadly (not just the bind-lowest) because the part that sits
            // lowest depends on the pose: the Skeleton's bind pose is a forward bow, so
            // its bind-lowest vertices are the skull, while in the walk/idle pose the
            // feet are lowest. Sampling across the whole body covers every extremity.
            let mut all: Vec<GroundProbe> = Vec::new();
            for (i, (mesh, sd_opt)) in meshes.iter().zip(skin_meshes.iter()).enumerate() {
                if (skinned_mesh_scales[i] - skinned_node_scale).abs() >= skinned_node_scale * 0.5 {
                    continue;
                }
                let Some(smesh) = sd_opt else { continue };
                for (vi, pos) in mesh.positions.iter().enumerate() {
                    let joints  = smesh.joint_indices.get(vi).copied().unwrap_or([0; 4]);
                    let weights = smesh.joint_weights.get(vi).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
                    all.push(GroundProbe { pos: *pos, joints, weights });
                }
            }
            // Evenly stride to cap the per-frame skinning cost while covering the body.
            const MAX_PROBES: usize = 400;
            let stride = (all.len() / MAX_PROBES).max(1);
            sd.ground_probes = all.into_iter().step_by(stride).collect();
        }

        // Compute bounds from dominant-scale meshes only (skips accessory meshes like weapons
        // whose node_scale differs from the body, preventing inflated lift values).
        // For static models all entries in skinned_mesh_scales are 1.0 so the filter always passes.
        let dominant_positions: Vec<[f32; 3]> = meshes.iter().zip(skinned_mesh_scales.iter())
            .filter(|(_, &ms)| (ms - skinned_node_scale).abs() < skinned_node_scale * 0.5)
            .flat_map(|(m, _)| m.positions.iter().copied())
            .collect();

        let y_min = dominant_positions.iter().map(|p| p[1]).fold(f32::MAX, f32::min);
        let y_max = dominant_positions.iter().map(|p| p[1]).fold(f32::MIN, f32::max);
        let y_bottom = if y_min < 0.0 { -y_min } else { 0.0 };
        let y_extent = if y_min < f32::MAX && y_max > f32::MIN { y_max - y_min } else { 0.0 };

        // Horizontal recentre offsets. `x_center`/`z_center` are the two non-height axes
        // in the load-order the render matrix expects (see entity_model_matrix_heading).
        //   - Static models keep their raw Y-up vertices: horizontal axes are raw X and Z.
        //   - Skinned models are also Y-up (height = Y); their horizontal axes are the
        //     skinned X and Z. Measure the posed (bind) skin points so attachment/eye
        //     pieces don't skew the centre.
        let (x_center, z_center) = if let Some(sd) = skin_data.as_ref() {
            let skin = sd.bind_skin_matrices();
            let (mut xmin, mut xmax, mut zmin, mut zmax) =
                (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
            for (i, (mesh, sd_opt)) in meshes.iter().zip(skin_meshes.iter()).enumerate() {
                if (skinned_mesh_scales[i] - skinned_node_scale).abs() >= skinned_node_scale * 0.5 {
                    continue;
                }
                let Some(smesh) = sd_opt else { continue };
                for (vi, pos) in mesh.positions.iter().enumerate() {
                    let joints  = smesh.joint_indices.get(vi).copied().unwrap_or([0; 4]);
                    let weights = smesh.joint_weights.get(vi).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
                    let p = crate::anim::SkinData::skin_point(*pos, joints, weights, &skin);
                    if p[0].is_finite() && p[2].is_finite() {
                        xmin = xmin.min(p[0]); xmax = xmax.max(p[0]);
                        zmin = zmin.min(p[2]); zmax = zmax.max(p[2]);
                    }
                }
            }
            if xmin <= xmax { ((xmin + xmax) * 0.5, (zmin + zmax) * 0.5) } else { (0.0, 0.0) }
        } else {
            let x_min = dominant_positions.iter().map(|p| p[0]).fold(f32::MAX, f32::min);
            let x_max = dominant_positions.iter().map(|p| p[0]).fold(f32::MIN, f32::max);
            let z_min = dominant_positions.iter().map(|p| p[2]).fold(f32::MAX, f32::min);
            let z_max = dominant_positions.iter().map(|p| p[2]).fold(f32::MIN, f32::max);
            if dominant_positions.is_empty() { (0.0, 0.0) }
            else { ((x_min + x_max) * 0.5, (z_min + z_max) * 0.5) }
        };

        // Finalize true_height: prefer eq_height from extras; fall back to measured y_extent.
        let true_height = if eq_height_from_extras > 0.0 { eq_height_from_extras } else { y_extent };

        // Per-clip posed bounds: recenter + ground from the CURRENT clip, not the bind pose.
        // center axes (p0,p2) match x_center/z_center; floor (min p1) matches bind_lowest_skinned_z.
        // Floor is the min over several sample times so it's stable within a clip (no walk bob).
        let clip_bounds: Vec<(f32, f32, f32)> = if let Some(sd) = skin_data.as_ref() {
            sd.clips.iter().enumerate().map(|(ci, clip)| {
                let mats: Vec<glam::Mat4> = sd.evaluate(ci, 0.0).iter()
                    .map(|m| glam::Mat4::from_cols_array_2d(m)).collect();
                let (mut xmin, mut xmax, mut zmin, mut zmax) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
                for (i, (mesh, sd_opt)) in meshes.iter().zip(skin_meshes.iter()).enumerate() {
                    if (skinned_mesh_scales[i] - skinned_node_scale).abs() >= skinned_node_scale * 0.5 { continue; }
                    let Some(smesh) = sd_opt else { continue };
                    for (vi, pos) in mesh.positions.iter().enumerate() {
                        let joints  = smesh.joint_indices.get(vi).copied().unwrap_or([0; 4]);
                        let weights = smesh.joint_weights.get(vi).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
                        let p = crate::anim::SkinData::skin_point(*pos, joints, weights, &mats);
                        if p[0].is_finite() && p[2].is_finite() {
                            xmin = xmin.min(p[0]); xmax = xmax.max(p[0]);
                            zmin = zmin.min(p[2]); zmax = zmax.max(p[2]);
                        }
                    }
                }
                let cx = if xmin <= xmax { (xmin + xmax) * 0.5 } else { x_center };
                let cz = if zmin <= zmax { (zmin + zmax) * 0.5 } else { z_center };
                let dur = clip.duration.max(0.0001);
                let floor = (0..6).map(|k| sd.lowest_skinned_z(ci, dur * (k as f32) / 6.0))
                    .fold(f32::MAX, f32::min);
                (cx, cz, floor)
            }).collect()
        } else { vec![] };

        // From the IDLE pose (what's actually rendered) measure two things over the dominant
        // body meshes' model-Y:
        //   - feet_offset = 5th percentile (robust feet; excludes stray geometry below the feet)
        //   - idle_extent = full vertical extent
        // Scaling by the idle extent (rather than eq_height = the BIND-pose extent) makes every
        // model render at its archetype target height. eq_height is wrong when the idle pose
        // differs from bind — e.g. a bat with wings spread (bind 3 → idle 15 → 5x oversized).
        let (feet_offset, idle_extent): (f32, f32) = match skin_data.as_ref() {
            Some(sd) if !sd.clips.is_empty() => {
                let idle = sd.clip_for_action("idle")
                    .or_else(|| sd.clip_for_action("walking")).unwrap_or(0);
                let mats: Vec<glam::Mat4> = sd.evaluate(idle, 0.0).iter()
                    .map(|m| glam::Mat4::from_cols_array_2d(m)).collect();
                let mut ys: Vec<f32> = Vec::new();
                for (i, (mesh, sd_opt)) in meshes.iter().zip(skin_meshes.iter()).enumerate() {
                    if (skinned_mesh_scales[i] - skinned_node_scale).abs() >= skinned_node_scale * 0.5 { continue; }
                    let Some(smesh) = sd_opt else { continue };
                    for (vi, pos) in mesh.positions.iter().enumerate() {
                        let joints  = smesh.joint_indices.get(vi).copied().unwrap_or([0; 4]);
                        let weights = smesh.joint_weights.get(vi).copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
                        let y = crate::anim::SkinData::skin_point(*pos, joints, weights, &mats)[1];
                        if y.is_finite() { ys.push(y); }
                    }
                }
                if ys.is_empty() { (0.0, 0.0) } else {
                    ys.sort_by(|a, b| a.partial_cmp(b).unwrap());
                    (ys[((ys.len() - 1) as f32 * 0.05) as usize], ys[ys.len() - 1] - ys[0])
                }
            }
            _ => (0.0, 0.0),
        };
        // Prefer the measured idle extent for scaling; fall back to eq_height/measured bounds.
        let true_height = if idle_extent > 0.001 { idle_extent } else { true_height };

        Ok(ModelAsset { meshes, textures, skin: skin_data, skin_meshes, skinned_node_scale, skinned_mesh_scales, y_bottom, y_extent, x_center, z_center, prefix: model_prefix, equip_slots, true_height, clip_bounds, feet_offset })
    }

    /// Load a static character model from an EQ `_chr.s3d` archive.
    /// Uses the WLD meshes and BMP/DDS textures inside the archive.
    /// Returns a ModelAsset with no skin data (static mesh, no animation).
    pub fn load_from_chr_s3d(s3d_path: &Path) -> Result<Self> {
        let assets = crate::assets::ZoneAssets::load(s3d_path)
            .with_context(|| format!("failed to load chr S3D: {}", s3d_path.display()))?;

        if assets.terrain.is_empty() {
            anyhow::bail!("no meshes found in {}", s3d_path.display());
        }

        // Compute y_bottom and y_extent from all mesh positions (world space).
        let mut y_min = f32::MAX;
        let mut y_max = f32::MIN;
        for m in &assets.terrain {
            for &p in &m.positions {
                let wy = p[1] + m.center[1]; // libeq: [east, height, north]
                if wy < y_min { y_min = wy; }
                if wy > y_max { y_max = wy; }
            }
        }
        let y_bottom = if y_min == f32::MAX { 0.0 } else { y_min };
        let y_extent = if y_min < f32::MAX && y_max > f32::MIN { y_max - y_min } else { 0.0 };

        // Compute x/z center from all mesh positions.
        let (mut x_min, mut x_max) = (f32::MAX, f32::MIN);
        let (mut z_min, mut z_max) = (f32::MAX, f32::MIN);
        for m in &assets.terrain {
            for &p in &m.positions {
                let wx = p[0] + m.center[0];
                let wz = p[2] + m.center[2];
                if wx < x_min { x_min = wx; }
                if wx > x_max { x_max = wx; }
                if wz < z_min { z_min = wz; }
                if wz > z_max { z_max = wz; }
            }
        }
        let x_center = if x_min <= x_max { (x_min + x_max) * 0.5 } else { 0.0 };
        let z_center = if z_min <= z_max { (z_min + z_max) * 0.5 } else { 0.0 };

        let mesh_count = assets.terrain.len();
        let tex_count = assets.textures.len();
        tracing::info!("models: loaded chr model from {} ({} meshes, {} textures)",
                  s3d_path.display(), mesh_count, tex_count);

        Ok(ModelAsset {
            meshes:            assets.terrain,
            textures:          assets.textures,
            skin:              None,
            skin_meshes:       vec![],
            skinned_node_scale: 1.0,
            skinned_mesh_scales: vec![1.0; mesh_count],
            y_bottom,
            y_extent,
            x_center,
            z_center,
            prefix: String::new(),
            equip_slots: vec![None; mesh_count],
            // chr.s3d models have no glTF extras; fall back to measured y_extent.
            true_height: y_extent,
            clip_bounds: vec![], // static chr.s3d models have no clips
            feet_offset: 0.0,
        })
    }
}

/// One body region's equipment-slot binding for a single mesh primitive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EquipSlot {
    /// Equipment array index (0=head .. 6=feet).
    pub slot: usize,
    /// Lowercase 2-char body region code, e.g. `*b"ch"`.
    pub region: [u8; 2],
    /// Piece/variant number within the region.
    pub variant: u8,
}

/// Map a 2-char body region code (case-insensitive) to an equipment slot index.
pub fn region_to_slot(region: &str) -> Option<usize> {
    match region.to_ascii_uppercase().as_str() {
        "HE" => Some(0),
        "CH" => Some(1),
        "UA" => Some(2),
        "FA" => Some(3),
        "HN" => Some(4),
        "LG" => Some(5),
        "FT" => Some(6),
        _ => None,
    }
}

/// Parse a glTF material name like `HOMCH0001_MDF` into its lowercase race+gender
/// prefix and the equipment slot it belongs to. Returns `None` for non-armor
/// materials (eyes, attachments) or malformed names.
pub fn parse_equip_material(name: &str) -> Option<(String, EquipSlot)> {
    let core = name.strip_suffix("_MDF").unwrap_or(name);
    if !core.is_ascii() || core.len() < 9 {
        return None;
    }
    let prefix = &core[0..3];
    let region = &core[3..5];
    let digits = &core[5..9];
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let slot = region_to_slot(region)?;
    let variant: u8 = digits[2..4].parse().ok()?;
    let mut rc = [0u8; 2];
    rc.copy_from_slice(region.to_ascii_lowercase().as_bytes());
    Some((prefix.to_ascii_lowercase(), EquipSlot { slot, region: rc, variant }))
}

/// Build the lowercase armor texture base name (no extension) for a swap.
pub fn equip_texture_name(prefix: &str, region: &[u8; 2], material: u32, variant: u8) -> String {
    let region = std::str::from_utf8(region).unwrap_or("");
    format!("{}{}{:02}{:02}", prefix, region, material, variant)
}

/// The armor-texture key for an equipped body slot, or `None` when the model's own
/// baked texture should be used. Returns `None` for **material 0** (naked/default — the
/// GLB bakes the skin texture, which does NOT match the numeric `..00..` name, so swapping
/// material 0 would wrongly blank head/feet) and for models with no race prefix.
/// Single source of truth shared by the render pass and the texture pre-pass.
pub fn equip_swap_key(prefix: &str, slot: EquipSlot, material: u32) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    // Material 0 = empty slot. For SKIN regions (head/hands/feet) that's bare skin → use the baked
    // face/hands/feet texture (None). For BODY regions (chest/arms/forearm/legs) material 0 is the
    // baseline CLOTH texture (variant 00, e.g. "elfch0001" — a clothed tunic), NOT skin: load it by
    // name from the s3d like the original client does. EQ has no nude-torso texture, and the GLB's
    // baked body texture is unreliable (it baked skin). See original-client
    // analysis of the original client ("why a brand-new character is not naked").
    if material == 0 && matches!(&slot.region, b"he" | b"hn" | b"ft") {
        return None;
    }
    Some(equip_texture_name(prefix, &slot.region, material, slot.variant))
}

/// Velious armor materials (17-23) reuse a classic base-tier texture when a race's Velious art isn't
/// shipped (e.g. elves have no `elf*23` textures, only human/Iksar do). The original Titanium client
/// remaps them (eqgame.exe client_fn): 17/20/23 → 1 (leather), 18/21 → 2 (chain), 19/22 → 3
/// (plate). Returns the fallback material to try when the raw material's texture is missing, so e.g.
/// material-23 cloth pants on an elf render as leather-look leggings instead of bare skin. (The
/// wizard-only 23 → 4 case is omitted.)
pub fn velious_material_fallback(material: u32) -> Option<u32> {
    if (17..=23).contains(&material) {
        Some(((material - 17) % 3) + 1)
    } else {
        None
    }
}

/// Map an EQ race string (case-insensitive) to a glTF archetype key.
pub fn race_to_archetype(race: &str) -> &'static str {
    match race.to_uppercase().as_str() {
        "HUM" | "HFL" | "GNM" | "ERU" |
        "IKS" | "VAH" | "BAR" | "TRL" | "OGR"          => "humanoid",
        "ELF" | "HEF" | "DKE"                           => "elf",
        "DWF"                                            => "dwarf",
        "GNL" | "KOB" | "GOB" | "ORC"                   => "gnoll",
        "SKE"                                            => "skeleton",
        "ZOM"                                            => "zombie",
        "SPI" | "BUG"                                    => "creature",  // spider
        "BEA"                                            => "bear",
        "WOL" | "LIO" | "CAT"                           => "wolf",
        "RAT"                                            => "rat",
        "SNA"                                            => "snake",
        "FRG"                                            => "frog",
        "BAT"                                            => "bat",
        "BRD"                                            => "bird",
        "WSP" | "WAS"                                    => "wasp",
        "WRM"                                            => "worm",
        "FIS"                                            => "fish",
        _                                                => "creature",
    }
}

/// Map an archetype key to the EQ `_chr.s3d` filename (without path).
/// Returns `None` for archetypes that have no EQ character archive.
pub fn archetype_to_chr_s3d(archetype: &str) -> Option<&'static str> {
    match archetype {
        "humanoid"  => Some("globalhum_chr.s3d"),  // human male (globalhom is Halfling, not Human)
        "elf"       => Some("globalelf_chr.s3d"),   // wood elf
        "dwarf"     => Some("globaldwf_chr.s3d"),   // dwarf
        "gnoll"     => Some("globalgnm_chr.s3d"),   // gnoll
        "skeleton"  => None, // old WLD format (DmSprite), not convertible to glTF
        "frog"      => Some("globalfroglok_chr.s3d"),// froglok
        // global_chr.s3d is a combined archive (701 meshes for ALL races).
        // Loading it as a single model produces a giant combined bounding box.
        // These archetypes need per-race chr.s3d extraction to render correctly.
        "zombie"    => None,
        "rat"       => None,
        "snake"     => None,
        "bat"       => None,
        "wolf"      => None,
        "bear"      => None,
        _           => None,
    }
}

/// Fixed display scale (EQ units) for each glTF archetype.
/// All models with node_scale=100 have that applied during loading, so these
/// values reflect the effective post-scale model height in EQ units.
/// Arch-scale is a multiplier such that `visual_scale = 2 * y_bottom * arch_scale * node_scale`
/// equals the desired total character height in EQ units (feet-to-head).
/// Calibrated from actual GLTF vertex bounds; review after adding new models.
pub fn archetype_scale(archetype: &str) -> f32 {
    // EQ units ≈ feet. `height = y_extent * arch_scale` gives rendered model height.
    // Calibrated from actual GLB vertex bounds; review after adding new models.
    match archetype {
        "humanoid" =>  3.55, // y_extent=1.6902 → 6.0 EQ (human adult)
        "elf"      =>  5.21, // y_extent=1.1526 → 6.0 EQ (human height)
        "dwarf"    =>  2.55, // y_extent=1.7623 → 4.5 EQ (3/4 human)
        "gnoll"    =>  3.01, // y_extent=1.6613 → 5.0 EQ (medium monster)
        "skeleton" =>  3.55, // humanoid-scale undead
        "zombie"   =>  3.55, // humanoid-scale undead
        "creature" =>  0.45, // Wolf spider:     → ~2.4 EQ units
        "rat"      =>  0.27, // Rat:             → ~1.2 EQ units
        "snake"    =>  0.57, // Snake:           → ~1.8 EQ units
        "frog"     =>  0.53, // y_extent=2.8574  → 1.5 EQ (small)
        "wasp"     =>  0.63, // Wasp:            → ~1.5 EQ units
        "wolf"     =>  1.2,  // Wolf:            → ~3 EQ units
        "bat"      =>  0.57, // Bat:             → ~1.5 EQ units
        "bird"     =>  0.9,  // Pigeon:          → ~2 EQ units
        "worm"     =>  3.5,  // Worm:            → ~1.5 EQ units
        "fish"     =>  0.18, // Fish:            → ~1.2 EQ units
        "bear"     =>  8.0,  // Panda bear:      → ~5 EQ units
        _          =>  6.0,
    }
}

/// Target height in EQ units for each archetype, used to scale normalized skinned models
/// so that `true_height` maps to the correct in-world visual height.
/// EQ units ≈ feet. Values are initial calibration; fine-tune visually after loading.
pub fn archetype_target_height(archetype: &str) -> f32 {
    match archetype {
        // target == rendered EQ height; humanoid=12 calibrated to the doorway. Other
        // human-height races match it; the rest are proportional (visually tune later).
        "humanoid" => 12.0, "elf" => 12.0, "dwarf" => 9.0, "gnoll" => 12.0,
        "skeleton" => 12.0, "zombie" => 12.0, "frog" => 10.0,
        "bear" => 12.0, "wolf" => 8.0, "rat" => 3.0, "snake" => 6.0,
        "bat" => 4.0, "bird" => 4.0, "wasp" => 4.0, "worm" => 4.0,
        "fish" => 3.0, "creature" => 8.0,
        _ => 12.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_returns_err_on_missing_file() {
        let result = ModelAsset::load(Path::new("/nonexistent/model.glb"));
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "requires bundled model at eq_renderer/assets/models/humanoid.glb"]
    fn load_humanoid_has_meshes() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        assert!(!asset.meshes.is_empty(), "expected at least one mesh");
    }

    #[test]
    #[ignore = "requires bundled model at eq_renderer/assets/models/creature.glb"]
    fn load_creature_has_skin_and_clips() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/creature.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        let skin = asset.skin.expect("creature.glb must have a skin");
        assert!(skin.joint_count > 0, "expected joints");
        assert!(skin.joint_count <= 128, "too many joints for uniform buffer");
        assert!(!skin.clips.is_empty(), "expected animation clips");
        assert!(skin.clip_for_action("walking").is_some(), "no walking clip found");
    }

    #[test]
    #[ignore = "requires bundled model at eq_renderer/assets/models/humanoid.glb"]
    fn humanoid_has_walk_clip_and_node_scale() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        // EQ-converted models have node_scale 1.0 (the old ≈100 was the Quaternius/CC0
        // placeholder before the s3d_to_gltf pipeline). Just require a sane positive scale.
        assert!(asset.skinned_node_scale > 0.0 && asset.skinned_node_scale.is_finite(),
            "node_scale should be positive+finite, got {}", asset.skinned_node_scale);
        let skin = asset.skin.expect("humanoid must have a skin");
        assert!(skin.joint_count <= 128, "joint count {} exceeds shader limit", skin.joint_count);
        let idx = skin.clip_for_action("walking")
            .expect("no walk clip found; clip names may not contain 'walk'");
        let clip = &skin.clips[idx];
        assert!(clip.duration > 0.0, "walk clip has zero duration");
        assert!(!clip.channels.is_empty(), "walk clip has no channels");
    }

    #[test]
    #[ignore = "requires bundled model at eq_renderer/assets/models/humanoid.glb"]
    fn humanoid_evaluate_produces_finite_matrices() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        let skin = asset.skin.expect("humanoid must have a skin");
        let walk_idx = skin.clip_for_action("walking").expect("no walk clip");
        // Evaluate at several points through the clip
        for t in [0.0, 0.1, 0.5, skin.clips[walk_idx].duration * 0.5] {
            let mats = skin.evaluate(walk_idx, t);
            assert_eq!(mats.len(), skin.joint_count);
            for (j, mat) in mats.iter().enumerate() {
                for col in mat.iter() {
                    for &v in col.iter() {
                        assert!(v.is_finite(), "joint {j} has non-finite value {v} at t={t}");
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "requires bundled model at eq_renderer/assets/models/humanoid.glb"]
    fn humanoid_joint_indices_in_bounds() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        let joint_count = asset.skin.as_ref().map_or(0, |s| s.joint_count);
        for (m, sd) in asset.skin_meshes.iter().enumerate() {
            if let Some(sd) = sd {
                for (v, ji) in sd.joint_indices.iter().enumerate() {
                    for &idx in ji.iter() {
                        assert!(idx < joint_count as u32,
                            "mesh {m} vertex {v}: joint index {idx} >= joint_count {joint_count}");
                    }
                }
            }
        }
    }

    #[test]
    fn race_to_archetype_known_races() {
        assert_eq!(race_to_archetype("HUM"), "humanoid");
        assert_eq!(race_to_archetype("ELF"), "elf");
        assert_eq!(race_to_archetype("DWF"), "dwarf");
        assert_eq!(race_to_archetype("BEA"), "bear");
        assert_eq!(race_to_archetype("GNL"), "gnoll");
        assert_eq!(race_to_archetype("SKE"), "skeleton");
        assert_eq!(race_to_archetype("ZOM"), "zombie");
        assert_eq!(race_to_archetype("SPI"), "creature");
        assert_eq!(race_to_archetype("RAT"), "rat");
        assert_eq!(race_to_archetype("SNA"), "snake");
        assert_eq!(race_to_archetype("FRG"), "frog");
        assert_eq!(race_to_archetype("BAT"), "bat");
        assert_eq!(race_to_archetype("BRD"), "bird");
        assert_eq!(race_to_archetype("WSP"), "wasp");
        assert_eq!(race_to_archetype("WAS"), "wasp");
        assert_eq!(race_to_archetype("WOL"), "wolf");
        assert_eq!(race_to_archetype("WRM"), "worm");
        assert_eq!(race_to_archetype("FIS"), "fish");
        assert_eq!(race_to_archetype(""),    "creature");
        assert_eq!(race_to_archetype("UNKNOWN"), "creature");
    }

    /// End-to-end: EQ race id → archetype model. Guards the run-10 fixes to the NPC race
    /// table (Skeleton/Zombie/Wasp/Rat/Gnoll/Fish/Kobold were mapped to wrong creatures).
    #[test]
    fn npc_race_ids_map_to_sensible_archetypes() {
        use crate::eq_net::protocol::eq_race_to_code;
        let arch = |id: u32| race_to_archetype(eq_race_to_code(id));
        assert_eq!(arch(60), "skeleton");  // Skeleton (was fish)
        assert_eq!(arch(70), "zombie");    // Zombie (was bear)
        assert_eq!(arch(109), "wasp");     // Wasp (was frog)
        assert_eq!(arch(36), "rat");       // Giant Rat (was zombie)
        assert_eq!(arch(39), "gnoll");     // Gnoll (was skeleton)
        assert_eq!(arch(24), "fish");      // Fish (was creature/spider)
        assert_eq!(arch(48), "gnoll");     // Kobold (was unmapped "FLY" → creature)
        assert_eq!(arch(94), "dwarf");     // Kaladim Citizen (was creature/spider)
        assert_eq!(arch(34), "bat");       // Giant Bat (was humanoid)
        assert_eq!(arch(26), "frog");      // Froglok (was skeleton)
    }

    #[test]
    fn race_to_archetype_case_insensitive() {
        assert_eq!(race_to_archetype("hum"), "humanoid");
        assert_eq!(race_to_archetype("Gnl"), "gnoll");
        assert_eq!(race_to_archetype("rat"), "rat");
    }

    #[test]
    fn target_heights_are_sane() {
        assert!((archetype_target_height("humanoid") - 12.0).abs() < 0.01);
        assert!(archetype_target_height("dwarf") < archetype_target_height("humanoid"));
        assert!(archetype_target_height("unknown") > 0.0);
    }

    /// Deterministic check of the player-pass placement math: load the real human
    /// model, replicate the skinned-player transform, and assert the model ends up
    /// grounded (feet ≈ pos.z), horizontally centered on pos, and ~target tall.
    #[test]
    #[ignore = "requires assets/models/humanoid.glb"]
    fn humanoid_player_transform_grounds_and_centers() {
        let p = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb"));
        let a = ModelAsset::load(&p).expect("load");
        let sk = a.skin.as_ref().expect("skin");
        let target = archetype_target_height("humanoid");
        let height = if a.true_height > 0.001 { a.true_height } else { 1.0 };
        let ms = (target / height) * a.skinned_node_scale;
        let lift_basis = -sk.bind_lowest_skinned_z();
        let visual_scale = 2.0 * lift_basis * ms;
        let center_xz = [a.x_center, a.z_center];
        let pos = [100.0_f32, -200.0, 5.0];
        let mat = crate::camera::entity_model_matrix_heading(
            pos, 0.0, visual_scale, ms, center_xz, true, 0.0);
        let m = glam::Mat4::from_cols_array_2d(&mat);
        let skin = sk.bind_skin_matrices();
        let (mut mnx, mut mxx) = (f32::MAX, f32::MIN);
        let (mut mny, mut mxy) = (f32::MAX, f32::MIN);
        let (mut mnz, mut mxz) = (f32::MAX, f32::MIN);
        for (mesh, sdo) in a.meshes.iter().zip(a.skin_meshes.iter()) {
            if let Some(sd) = sdo {
                for (vi, vp) in mesh.positions.iter().enumerate() {
                    let j = sd.joint_indices[vi];
                    let w = sd.joint_weights[vi];
                    let local = crate::anim::SkinData::skin_point(*vp, j, w, &skin);
                    let wp = m.transform_point3(glam::Vec3::from(local));
                    mnx = mnx.min(wp.x); mxx = mxx.max(wp.x);
                    mny = mny.min(wp.y); mxy = mxy.max(wp.y);
                    mnz = mnz.min(wp.z); mxz = mxz.max(wp.z);
                }
            }
        }
        let (cx, cy, h) = ((mnx + mxx) * 0.5, (mny + mxy) * 0.5, mxz - mnz);
        tracing::info!("PLACEMENT world x[{:.2},{:.2}] y[{:.2},{:.2}] z[{:.2},{:.2}] center=({:.2},{:.2}) height={:.2} feet_z={:.2} (pos={:?} target={})",
            mnx, mxx, mny, mxy, mnz, mxz, cx, cy, h, mnz, pos, target);
        assert!((mnz - pos[2]).abs() < 1.5, "feet z {:.2} should be ~pos.z {:.2}", mnz, pos[2]);
        assert!((cx - pos[0]).abs() < 1.5, "x center {:.2} vs pos.x {:.2}", cx, pos[0]);
        assert!((cy - pos[1]).abs() < 1.5, "y center {:.2} vs pos.y {:.2}", cy, pos[1]);
        assert!((h - target).abs() < target * 0.3, "height {:.2} vs target {:.2}", h, target);
    }

    /// Same as above but for the ANIMATED idle pose using per-clip bounds — this is the
    /// case the live player renders. Guards the fix for the static-offset bug (the idle
    /// pose differs from bind, so bind-based recenter/grounding left the model offset).
    #[test]
    #[ignore = "requires assets/models/humanoid.glb"]
    fn humanoid_idle_pose_grounds_and_centers() {
        let p = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb"));
        let a = ModelAsset::load(&p).expect("load");
        let sk = a.skin.as_ref().expect("skin");
        let idle = sk.clip_for_action("idle").or_else(|| sk.clip_for_action("walking")).unwrap_or(0);
        let (cx, cz, floor) = a.clip_bounds[idle];
        let target = archetype_target_height("humanoid");
        let ms = (target / a.true_height) * a.skinned_node_scale;
        let visual_scale = 2.0 * (-floor) * ms;
        let pos = [100.0_f32, -200.0, 5.0];
        let mat = crate::camera::entity_model_matrix_heading(pos, 0.0, visual_scale, ms, [cx, cz], true, 0.0);
        let m = glam::Mat4::from_cols_array_2d(&mat);
        let imats: Vec<glam::Mat4> = sk.evaluate(idle, 0.0).iter()
            .map(|x| glam::Mat4::from_cols_array_2d(x)).collect();
        let (mut mnx, mut mxx, mut mny, mut mxy, mut mnz, mut mxz) =
            (f32::MAX, f32::MIN, f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for (mesh, sdo) in a.meshes.iter().zip(a.skin_meshes.iter()) {
            if let Some(sd) = sdo {
                for (vi, vp) in mesh.positions.iter().enumerate() {
                    let local = crate::anim::SkinData::skin_point(*vp, sd.joint_indices[vi], sd.joint_weights[vi], &imats);
                    let wp = m.transform_point3(glam::Vec3::from(local));
                    mnx = mnx.min(wp.x); mxx = mxx.max(wp.x);
                    mny = mny.min(wp.y); mxy = mxy.max(wp.y);
                    mnz = mnz.min(wp.z); mxz = mxz.max(wp.z);
                }
            }
        }
        let (ccx, ccy, h) = ((mnx + mxx) * 0.5, (mny + mxy) * 0.5, mxz - mnz);
        tracing::info!("IDLE world center=({:.2},{:.2}) feet_z={:.2} height={:.2} (pos={:?})", ccx, ccy, mnz, h, pos);
        assert!((mnz - pos[2]).abs() < 1.5, "idle feet z {:.2} should be ~pos.z {:.2}", mnz, pos[2]);
        assert!((ccx - pos[0]).abs() < 1.5, "idle x center {:.2} vs pos.x {:.2}", ccx, pos[0]);
        assert!((ccy - pos[1]).abs() < 1.5, "idle y center {:.2} vs pos.y {:.2}", ccy, pos[1]);
    }

    /// The per-clip positioning fix must generalize to every race/gender model the user
    /// sees on NPCs — not just the male human. Loads each present gendered model, evaluates
    /// its idle clip, and asserts the rendered pose grounds (feet≈pos.z) and centers (xy≈pos).
    #[test]
    #[ignore = "requires assets/models/*.glb"]
    fn gendered_models_idle_ground_and_center() {
        let pos = [100.0_f32, -200.0, 5.0];
        let mut checked = 0;
        for (name, archetype) in [
            ("humanoid", "humanoid"), ("humanoid_f", "humanoid"),
            ("elf", "elf"), ("elf_f", "elf"),
            ("dwarf", "dwarf"), ("dwarf_f", "dwarf"),
        ] {
            let path = std::path::PathBuf::from(
                format!("{}/assets/models/{}.glb", env!("CARGO_MANIFEST_DIR"), name));
            if !path.exists() { continue; }
            let a = ModelAsset::load(&path).expect("load");
            let sk = a.skin.as_ref().expect("skin");
            let idle = sk.clip_for_action("idle").or_else(|| sk.clip_for_action("walking")).unwrap_or(0);
            let (cxb, czb, floor) = a.clip_bounds[idle];
            let target = archetype_target_height(archetype);
            let ms = (target / a.true_height) * a.skinned_node_scale;
            let visual_scale = 2.0 * (-floor) * ms;
            let mat = crate::camera::entity_model_matrix_heading(pos, 0.0, visual_scale, ms, [cxb, czb], true, 0.0);
            let m = glam::Mat4::from_cols_array_2d(&mat);
            let imats: Vec<glam::Mat4> = sk.evaluate(idle, 0.0).iter()
                .map(|x| glam::Mat4::from_cols_array_2d(x)).collect();
            let (mut mnx, mut mxx, mut mny, mut mxy, mut mnz) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN, f32::MAX);
            for (mesh, sdo) in a.meshes.iter().zip(a.skin_meshes.iter()) {
                if let Some(sd) = sdo {
                    for (vi, vp) in mesh.positions.iter().enumerate() {
                        let local = crate::anim::SkinData::skin_point(*vp, sd.joint_indices[vi], sd.joint_weights[vi], &imats);
                        let wp = m.transform_point3(glam::Vec3::from(local));
                        mnx = mnx.min(wp.x); mxx = mxx.max(wp.x);
                        mny = mny.min(wp.y); mxy = mxy.max(wp.y);
                        mnz = mnz.min(wp.z);
                    }
                }
            }
            let (ccx, ccy) = ((mnx + mxx) * 0.5, (mny + mxy) * 0.5);
            tracing::info!("MODEL {name}: feet_z={:.2} center=({:.2},{:.2}) prefix={}", mnz, ccx, ccy, a.prefix);
            assert!((mnz - pos[2]).abs() < 1.5, "{name} feet z {:.2} vs pos.z {:.2}", mnz, pos[2]);
            assert!((ccx - pos[0]).abs() < 1.5, "{name} x center {:.2} vs pos.x {:.2}", ccx, pos[0]);
            assert!((ccy - pos[1]).abs() < 1.5, "{name} y center {:.2} vs pos.y {:.2}", ccy, pos[1]);
            checked += 1;
        }
        assert!(checked >= 1, "no gendered models found to check");
    }

    #[test]
    fn archetype_scale_returns_positive_for_all_archetypes() {
        assert!(archetype_scale("humanoid") > 0.0);
        assert!(archetype_scale("gnoll")   > 0.0);
        assert!(archetype_scale("skeleton") > 0.0);
        assert!(archetype_scale("humanoid") > 0.0);
        assert!(archetype_scale("gnoll")   > 0.0);
        assert!(archetype_scale("skeleton") > 0.0);
        assert!(archetype_scale("creature") > 0.0);
        assert!(archetype_scale("zombie")   > 0.0);
        assert!(archetype_scale("rat")      > 0.0);
        assert!(archetype_scale("snake")    > 0.0);
        assert!(archetype_scale("frog")     > 0.0);
        assert!(archetype_scale("wasp") > 0.0);
        assert!(archetype_scale("wolf") > 0.0);
        assert!(archetype_scale("bat")  > 0.0);
        assert!(archetype_scale("bird") > 0.0);
        assert!(archetype_scale("worm") > 0.0);
        assert!(archetype_scale("fish")  > 0.0);
        assert!(archetype_scale("bear")  > 0.0);
        assert!(archetype_scale("dwarf") > 0.0);
        assert!(archetype_scale("elf")   > 0.0);
        assert_eq!(archetype_scale("unknown"), 6.0);
    }

    #[test]
    fn region_to_slot_maps_all_armor_regions() {
        assert_eq!(region_to_slot("HE"), Some(0));
        assert_eq!(region_to_slot("CH"), Some(1));
        assert_eq!(region_to_slot("UA"), Some(2));
        assert_eq!(region_to_slot("FA"), Some(3));
        assert_eq!(region_to_slot("HN"), Some(4));
        assert_eq!(region_to_slot("LG"), Some(5));
        assert_eq!(region_to_slot("FT"), Some(6));
        assert_eq!(region_to_slot("ch"), Some(1)); // case-insensitive
        assert_eq!(region_to_slot("XX"), None);
    }

    #[test]
    fn parse_equip_material_chest() {
        let (prefix, es) = parse_equip_material("HOMCH0001_MDF").expect("should parse");
        assert_eq!(prefix, "hom");
        assert_eq!(es.slot, 1);
        assert_eq!(&es.region, b"ch");
        assert_eq!(es.variant, 1);
    }

    #[test]
    fn parse_equip_material_head_variant() {
        let (_, es) = parse_equip_material("HOMHE0007_MDF").unwrap();
        assert_eq!(es.slot, 0);
        assert_eq!(es.variant, 7);
    }

    #[test]
    fn parse_equip_material_rejects_non_armor() {
        assert!(parse_equip_material("HOFL_EYE_MDF").is_none());
        assert!(parse_equip_material("HOMR_01_MDF").is_none());
        assert!(parse_equip_material("short").is_none());
    }

    #[test]
    fn equip_texture_name_formats() {
        assert_eq!(equip_texture_name("hom", b"ch", 17, 1), "homch1701");
        assert_eq!(equip_texture_name("hom", b"ch", 0, 3),  "homch0003");
    }

    #[test]
    fn equip_swap_key_armor_returns_name() {
        let slot = EquipSlot { slot: 1, region: *b"ch", variant: 1 };
        assert_eq!(equip_swap_key("hom", slot, 17).as_deref(), Some("homch1701"));
    }

    #[test]
    fn equip_swap_key_material_zero_is_none() {
        // material 0 = naked → use the baked skin texture, NOT a constructed key
        // (this is the head/feet-disappearing bug fix).
        let slot = EquipSlot { slot: 0, region: *b"he", variant: 1 };
        assert_eq!(equip_swap_key("hom", slot, 0), None);
    }

    #[test]
    fn equip_swap_key_empty_prefix_is_none() {
        let slot = EquipSlot { slot: 1, region: *b"ch", variant: 1 };
        assert_eq!(equip_swap_key("", slot, 17), None);
    }

    #[test]
    #[ignore = "requires assets/models/humanoid.glb"]
    fn humanoid_true_height_from_extras() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        assert!(asset.true_height > 0.0,
            "true_height must be positive, got {}", asset.true_height);
        assert!(asset.true_height.is_finite(),
            "true_height must be finite, got {}", asset.true_height);
    }

    #[test]
    #[ignore = "requires assets/models/humanoid.glb"]
    fn humanoid_has_equip_slots_parallel_to_meshes() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb"));
        let asset = ModelAsset::load(&path).expect("load failed");
        assert_eq!(asset.equip_slots.len(), asset.meshes.len(),
            "equip_slots must be parallel to meshes");
        // The humanoid archetype must be the HUMAN model (prefix "hum"), not the Halfling
        // model "hom" — guards the wrong-source-archive regression (halfling feet on humans).
        assert_eq!(asset.prefix, "hum", "humanoid model must be human (hum), not halfling (hom)");
        assert!(asset.equip_slots.iter().flatten().any(|s| s.slot == 1),
            "expected at least one chest primitive");
    }

    #[test]
    #[ignore = "requires assets/models/humanoid.glb"]
    fn humanoid_clip_bounds_parallel_to_clips() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb"));
        let asset = ModelAsset::load(&path).expect("load failed");
        let skin = asset.skin.as_ref().expect("skinned humanoid");
        assert_eq!(asset.clip_bounds.len(), skin.clips.len(),
            "clip_bounds must be parallel to clips (recenter/grounding indexes by clip_idx)");
        assert!(asset.clip_bounds.iter().all(|(cx, cz, f)| cx.is_finite() && cz.is_finite() && f.is_finite()),
            "clip bounds must be finite");
    }

    #[test]
    #[ignore = "requires assets/models/humanoid.glb"]
    fn humanoid_mesh_count_fits_player_uniform_slots() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb"));
        let asset = ModelAsset::load(&path).expect("load failed");
        // The player pass draws one uniform slot per mesh and breaks past
        // PLAYER_UNIFORM_SLOTS — if the model has more meshes than slots, the player loses
        // its later primitives (head/feet). Guards the 16→32 slot-cap fix.
        assert!(asset.meshes.len() <= crate::renderer::PLAYER_UNIFORM_SLOTS,
            "humanoid has {} meshes but PLAYER_UNIFORM_SLOTS is {}",
            asset.meshes.len(), crate::renderer::PLAYER_UNIFORM_SLOTS);
    }
}



