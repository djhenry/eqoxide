//! Character model loading from glTF/GLB: meshes + per-vertex skin weights, textures, the skeleton
//! and animation clips, per-clip posed bounds (used to recenter + ground a model on its current
//! pose rather than its bind pose), and archetype scale. See `docs/character-models.md`.

use anyhow::{Context, Result};
use std::path::Path;
use eqoxide_assets::{MeshData, TextureData};
use crate::anim::{AnimClip, GroundProbe, JointChannel, JointProperty, SkinData};

/// A head primitive belonging to a Luclin head-region variant. RoF2 swaps the head regions
/// 1/4/5 (face+scalp, nose bridge, nose tip on humans; layout varies per race) by the spawn's
/// **face** value, which selects the head material set. Hairstyle is a dead actor-attach path
/// for S3D races (no
/// `*_HEAD_HAIR` actor ships in RoF2), so it selects nothing here. Hair itself is PAINTED
/// into these textures as a neutral light base, colored at runtime by the `haircolor` tint
/// table (see [`crate::head::hair_tint`]). The converter splits each region into a facial-skin
/// prim and a painted-hair scalp prim so only the hair texels get tinted.
///
/// Body, eyes, ears and the other fixed head regions carry no extras and are `None` in
/// `ModelAsset::head_parts` (always drawn, untinted).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadPart {
    /// Facial-skin part of face variant `F`; visible only when `face == F`, never tinted.
    /// Emitted as `{ "eq_face": F }`.
    Face(u8),
    /// Painted-hair scalp part, runtime-tinted by `haircolor`. `Some(F)` = the scalp half of
    /// face variant `F` (visible when `face == F`); `None` = an always-visible fixed hair
    /// region (e.g. the sculpted crown strip across the skull top). Emitted as
    /// `{ "eq_head_part": "hair" [, "eq_face": F] }`.
    Hair(Option<u8>),
}

/// Whether a mesh primitive should render given its head-part tag and the character's face.
/// - `None` (body, eyes, ears, fixed head regions): always visible.
/// - `Face(F)` / `Hair(Some(F))`: visible only when `face == F` (F=0 is the default).
/// - `Hair(None)` (fixed crown hair): always visible.
///
/// `_hairstyle` is retained in the signature (callers pass the spawn `hairstyle`) but is
/// unused: RoF2 ships no hairstyle geometry or textures for S3D player races, so hairstyle
/// has no visual effect on them (authentic client behavior). `_default_hidden` is likewise
/// unneeded — the default `face == 0` already selects the base variant via the match below.
pub fn head_part_visible(
    part: Option<HeadPart>,
    _default_hidden: bool,
    face: u8,
    _hairstyle: u8,
) -> bool {
    match part {
        None => true,
        Some(HeadPart::Face(f)) => f == face,
        Some(HeadPart::Hair(Some(f))) => f == face,
        Some(HeadPart::Hair(None)) => true,
    }
}

/// Runtime tint for a head primitive, as a multiplicative RGBA (1.0 = no change), or `None` to
/// keep the mesh's own base/equipment tint. Only painted-hair scalp parts ([`HeadPart::Hair`])
/// are ever tinted — by the character's `haircolor` via [`crate::head::hair_tint`] — and only
/// for the race/gender subset the native RoF2 client tints
/// ([`crate::head::hair_tint_applies`]: HIE/DKE/HEF + female DWF). For every other race a hair
/// prim returns explicit WHITE (drawn untinted, and never inheriting an equipment tint):
/// tinting e.g. HUM multiplied the skin-toned scalp/eye-band texels by a near-black brown and
/// produced #519's "raccoon-mask". `haircolor >= 24` → white for tinted races too (the
/// authentic neutral base). Facial skin and body parts are never tinted (`None`).
pub fn head_part_tint(
    part: Option<HeadPart>,
    haircolor: u8,
    race: &str,
    gender: u8,
) -> Option<[f32; 4]> {
    match part {
        Some(HeadPart::Hair(_)) => {
            if crate::head::hair_tint_applies(race, gender) {
                let t = crate::head::hair_tint(haircolor);
                Some([t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0])
            } else {
                Some([1.0, 1.0, 1.0, 1.0])
            }
        }
        _ => None,
    }
}

/// Classify a primitive's glTF `extras` object into a head-part tag + its default-hidden flag,
/// or `None` for untagged primitives (body/eyes/ears/fixed skin regions → always drawn).
///
/// Contract (asset-server face-variant bake):
/// - `{ "eq_face": F }` → [`HeadPart::Face`] (facial skin of face variant F).
/// - `{ "eq_face": F, "eq_head_part": "hair" }` → [`HeadPart::Hair`]`(Some(F))` (tinted scalp).
/// - `{ "eq_head_part": "hair" }` alone → [`HeadPart::Hair`]`(None)` (fixed crown hair).
///
/// Legacy GLBs (pre-fix) tagged the same textures `eq_hairstyle` — accepted as the face index
/// so old bakes still select a single variant instead of rendering all 8 overlapped.
pub(crate) fn parse_head_extras(v: &serde_json::Value) -> Option<(HeadPart, bool)> {
    let face = v
        .get("eq_face")
        .or_else(|| v.get("eq_hairstyle"))
        .and_then(|f| f.as_u64())
        .map(|f| f as u8);
    let is_hair = v.get("eq_head_part").and_then(|p| p.as_str()) == Some("hair");
    let dflt_hidden = v.get("eq_default_hidden").and_then(|b| b.as_bool()).unwrap_or(false);
    let part = match (is_hair, face) {
        (true, f) => HeadPart::Hair(f),
        (false, Some(f)) => HeadPart::Face(f),
        (false, None) => return None,
    };
    Some((part, dflt_hidden))
}

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
    /// Read here as the `true_height` fallback when the glTF carries no `eq_height` extra. It is
    /// NOT part of static placement: since #768 `static_placement` takes the whole lift from
    /// `y_bottom`. `visual_scale = 2 × y_extent × arch_scale` survives only in the standalone
    /// `render_model` viewer bin (`src/bin/render_model.rs:1097`).
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
    /// Per-mesh head-appearance tag, parallel to `meshes`. `None` = body/eyes (always visible).
    pub head_parts: Vec<Option<HeadPart>>,
    /// Per-mesh default-hidden flag from the converter's `eq_default_hidden` extras field.
    /// Parallel to `meshes`. Used alongside `head_parts` by `head_part_visible`.
    pub head_default_hidden: Vec<bool>,
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

/// Reduce a model's measured Y bounds (over its dominant-scale vertices) to the two quantities
/// `ModelAsset::load` publishes: `y_bottom` (the static-arm grounding lift, `StaticPlacement`'s
/// only lift term since #768) and `y_extent` (the plain vertical span, kept for `true_height` and
/// the standalone model-viewer bin, but explicitly NOT fed into static placement — see
/// `static_placement`'s doc comment).
///
/// This is a NAMED, single-call-site reduction rather than the inline arithmetic it replaces
/// (eqoxide#779), specifically so a test can call it directly with hand-known bounds — no glTF,
/// no GPU, no real asset. The formula it must not drift into: `y_bottom = -y_min + y_extent`,
/// which is `-y_min + (y_max - y_min)` — algebraically `y_bottom_correct + y_extent` — is #768's
/// exact over-lift reintroduced one file upstream of where #768/#773 fixed it, because a static
/// model's whole lift is `y_bottom * mesh_scale` and folding `y_extent` into `y_bottom` puts the
/// extent back into that product by another route.
///
/// **Spec, restated as the property that distinguishes this from that corruption:** `y_bottom`
/// is a function of `y_min` ALONE. It does not read `y_max` (nor, equivalently, `y_extent`) at
/// all. `tests::y_bottom_and_extent_hold_the_spec_over_many_generated_bounds` asserts exactly
/// this — that changing `y_max` while holding `y_min` fixed never changes `y_bottom` — over many
/// generated `(y_min, y_max)` pairs, which is strictly stronger than checking one fixture.
/// `tests::y_bottom_matches_the_intended_quantity_for_a_known_model` is the specific eqoxide#779
/// regression case (boat.glb's real measured bounds), mutation-checked both ways.
fn y_bottom_and_extent(y_min: f32, y_max: f32) -> (f32, f32) {
    let y_bottom = if y_min < 0.0 { -y_min } else { 0.0 };
    let y_extent = if y_min < f32::MAX && y_max > f32::MIN { y_max - y_min } else { 0.0 };
    (y_bottom, y_extent)
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
            let mut joint_names       = vec![String::new(); joint_count];
            for node in document.nodes() {
                if let Some(&ji) = joint_index_map.get(&node.index()) {
                    let (t, r, s) = node.transform().decomposed();
                    rest_translations[ji] = t;
                    rest_rotations[ji]    = r;
                    rest_scales[ji]       = s;
                    joint_names[ji] = node.name().unwrap_or("").to_uppercase();
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
                                ground_probes: Vec::new(), joint_names };
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
        let mut head_parts: Vec<Option<HeadPart>> = Vec::new();
        let mut head_default_hidden: Vec<bool> = Vec::new();
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
                    render_mode: eqoxide_assets::RenderMode::Opaque, anim: None,
                });
                skin_meshes.push(sd_opt);
                skinned_mesh_scales.push(this_mesh_scale);
                let parsed = primitive.material().name().and_then(parse_equip_material);
                if model_prefix.is_empty() {
                    if let Some((ref p, _)) = parsed { model_prefix = p.clone(); }
                }
                equip_slots.push(parsed.map(|(_, s)| s));

                // Parse head-part extras. Two contracts coexist:
                //  • asset-server #8 synthetic hair SHELLS: `{ "eq_head_part": "hair",
                //    "eq_hairstyle": H, "eq_default_hidden": true }` → `HeadPart::Hair(H)`
                //    (runtime-tinted by haircolor, hidden under a helm).
                //  • classic swappable scalp textures: plain `{ "eq_hairstyle": H }` (no
                //    `eq_head_part`) → `HeadPart::HairstyleVariant(H)` (color baked in, untinted).
                // Untagged primitives (body/eyes/ears/fixed head, and the always-visible bald base
                // scalp under the new contract) stay `None` and always render, untinted.
                let head_tag: Option<(HeadPart, bool)> = primitive.extras().as_ref().and_then(|ex| {
                    let v: serde_json::Value = serde_json::from_str(ex.get()).ok()?;
                    parse_head_extras(&v)
                });
                head_parts.push(head_tag.map(|(p, _)| p));
                head_default_hidden.push(head_tag.map(|(_, h)| h).unwrap_or(false));
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
        let (y_bottom, y_extent) = y_bottom_and_extent(y_min, y_max);

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
        //   - idle_extent = ROBUST vertical extent (0.5th..99.5th percentile)
        // Scaling by the idle extent (rather than eq_height = the BIND-pose extent) makes every
        // model render at its archetype target height. eq_height is wrong when the idle pose
        // differs from bind — e.g. a bat with wings spread (bind 3 → idle 15 → 5x oversized).
        // The extent MUST be robust to outliers: some models (notably the male Human/Barbarian/
        // Erudite Luclin meshes) carry a handful of stray vertices far above the head, so the raw
        // max-min is ~2× the real body. Using the full extent there halves the visible body
        // (true_height inflated → scale halved). The 0.5/99.5 percentiles track the body and drop
        // the strays. (Verified: race_hum body = p0.5..p99.5 ≈ 6.5, full max-min ≈ 11.6.)
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
                    let last = (ys.len() - 1) as f32;
                    let p = |q: f32| ys[(last * q) as usize];
                    // feet_offset: 5th pct (robust low). extent: 0.5th..99.5th pct (drops strays).
                    (p(0.05), p(0.995) - p(0.005))
                }
            }
            _ => (0.0, 0.0),
        };
        // Prefer the measured idle extent for scaling; fall back to eq_height/measured bounds.
        let true_height = if idle_extent > 0.001 { idle_extent } else { true_height };

        Ok(ModelAsset { meshes, textures, skin: skin_data, skin_meshes, skinned_node_scale, skinned_mesh_scales, y_bottom, y_extent, x_center, z_center, prefix: model_prefix, equip_slots, head_parts, head_default_hidden, true_height, clip_bounds, feet_offset })
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
    // baked body texture is unreliable (it baked skin). This matches the behavior of the
    // original Titanium game client ("why a brand-new character is not naked").
    if material == 0 && matches!(&slot.region, b"he" | b"hn" | b"ft") {
        return None;
    }
    Some(equip_texture_name(prefix, &slot.region, material, slot.variant))
}

/// Velious armor materials (17-23) reuse a classic base-tier texture when a race's Velious art isn't
/// shipped (e.g. elves have no `elf*23` textures, only human/Iksar do). The original Titanium client
/// remaps them (observed behavior of the original Titanium client): 17/20/23 → 1 (leather), 18/21 → 2 (chain), 19/22 → 3
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
        "IKS" | "VAH" | "BAR" | "TRL" | "OGR" | "DRK"  => "humanoid",
        "ELF" | "HIE" | "HEF" | "DKE"                   => "elf",
        "DWF"                                            => "dwarf",
        "SHP"                                            => "boat",   // boats/ships (#194)
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

/// Per-archetype model-space orientation fix-up applied in `entity_model_matrix_heading` (#149).
/// Most models need none (identity). The shared substitute `fish.glb` is authored with its
/// nose→tail along the model's Y axis; after the standard Y-up→Z-up conversion that axis points
/// straight up (world +Z) with the mouth at −Z, so the fish stands on its nose ("mouth-down").
/// Rotating −90° about Y lays it flat and turns the mouth to +X — the canonical "front = +X" pose
/// the heading yaw then points. (Verified in `--testzone`: the fish goes from a vertical sliver to a
/// horizontal, nose-forward fish.)
pub fn archetype_correction(archetype: &str) -> glam::Mat4 {
    match archetype {
        "fish" => glam::Mat4::from_rotation_y(-std::f32::consts::FRAC_PI_2),
        _ => glam::Mat4::IDENTITY,
    }
}

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
        // Boats/ships: the EQG model is already authored in EQ units (rowboat ~10u tall), so
        // render at ~native size rather than shrinking to a character height (#194).
        "boat"     =>  1.0,
        _          =>  6.0,
    }
}

/// Target rendered height in **EQ world feet** for each archetype, used to scale
/// normalized skinned models so the model's `true_height` maps to this in-world
/// height. EQ world units ARE feet — the same space as zone/terrain/door geometry
/// — so the value here is the character's literal height (a 6 ft human fits under a
/// ~7-8 ft doorway). Only the monster archetypes use this now; playable races take
/// their height from [`race_target_height`].
pub fn archetype_target_height(archetype: &str) -> f32 {
    match archetype {
        // Rendered height in EQ feet. Human-ish NPC archetypes are 6.0 (matches a
        // default human); the rest are roughly proportional (visually tune later).
        "humanoid" => 6.0, "elf" => 6.0, "dwarf" => 4.5, "gnoll" => 6.0,
        "skeleton" => 6.0, "zombie" => 6.0, "frog" => 5.0,
        "bear" => 6.0, "wolf" => 4.0, "rat" => 1.5, "snake" => 3.0,
        "bat" => 2.0, "bird" => 2.0, "wasp" => 2.0, "worm" => 2.0,
        "fish" => 1.5, "creature" => 4.0,
        _ => 6.0,
    }
}

/// Per-race rendered height in **EQ world feet**, for the **playable** races.
/// These are exactly EQEmu's `GetRaceGenderDefaultHeight` (`common/races.cpp`),
/// which the Titanium client uses as the base height for player models (the wire
/// `size` field is 0 for player spawns, so the client substitutes the race
/// default). EQ world units are feet — the same space as zone/terrain/door
/// geometry — so a character renders at exactly this height (a 6 ft human fits
/// under a ~7-8 ft doorway). NO display-unit multiplier.
///
/// Male and female share the same base height (no gender modifier in the table).
///
/// Keyed on the 3-letter race code from `eq_race_to_code`, where High Elf is
/// `"HIE"` and Half Elf is `"HEF"`.
///
/// Returns `None` for non-playable races (monsters), whose height comes from
/// [`archetype_target_height`] instead.
pub fn race_target_height(race: &str) -> Option<f32> {
    // EQ feet (== GetRaceGenderDefaultHeight; world units are feet).
    Some(match race.to_uppercase().as_str() {
        "HUM" => 6.0, // Human       6.0 ft
        "BAR" => 7.0, // Barbarian   7.0 ft
        "ERU" => 6.0, // Erudite     6.0 ft
        "ELF" => 5.0, // Wood Elf    5.0 ft
        "HIE" => 6.0, // High Elf    6.0 ft
        "HEF" => 5.5, // Half Elf    5.5 ft
        "DKE" => 5.0, // Dark Elf    5.0 ft
        "DWF" => 4.0, // Dwarf       4.0 ft
        "TRL" => 8.0, // Troll       8.0 ft
        "OGR" => 9.0, // Ogre        9.0 ft
        "HFL" => 3.5, // Halfling    3.5 ft
        "GNM" => 3.0, // Gnome       3.0 ft
        "FRG" => 5.0, // Froglok     5.0 ft
        "IKS" => 6.0, // Iksar       6.0 ft
        "VAH" => 7.0, // Vah Shir    7.0 ft
        _ => return None,
    })
}

/// Rendered height in display units for a spawn of the given race code: the
/// playable-race default ([`race_target_height`]) when known, else the archetype
/// fallback ([`archetype_target_height`]) for monsters.
pub fn target_height_for(race: &str, archetype: &str) -> f32 {
    race_target_height(race).unwrap_or_else(|| archetype_target_height(archetype))
}

/// The scale + vertical-lift (`visual_scale`) for placing a skinned humanoid/NPC model,
/// as the live render path uses them. This is the SINGLE source of that math: `src/pass.rs`
/// (production) and the placement regression test both call it, so the test guards the
/// ACTUAL render scale rather than a hand-copied formula (a divergent copy passed even when
/// a 2× bug was injected into the renderer — #357 review).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HumanoidPlacement {
    /// Uniform mesh scale fed to `entity_model_matrix_heading`'s `mesh_scale`.
    pub mesh_scale: f32,
    /// Z-lift fed to `entity_model_matrix_heading`'s `visual_scale` (grounds the feet).
    pub visual_scale: f32,
}

/// Compute the skinned-model placement scale/lift from the model's measured
/// `true_height` (robust idle extent), `feet_offset` (robust idle feet), and the archetype
/// `target` height.
///
/// - `mesh_scale = target / true_height` normalizes the robust body extent to `target`. Do
///   NOT re-apply the model's authored node_scale: skinned verts are stored raw and
///   `true_height` is measured from those same points, so `target/height` is already exact
///   (re-applying re-inflated the scale-100 `fish.glb` ~100×, #149).
/// - `visual_scale = -2 * feet_offset * mesh_scale` grounds the model by its own robust feet
///   (the origin sits above the feet; `entity_model_matrix_heading` lifts by
///   `visual_scale * 0.5`).
pub fn humanoid_placement(true_height: f32, feet_offset: f32, target: f32) -> HumanoidPlacement {
    let height = if true_height > 0.001 { true_height } else { 1.0 };
    let mesh_scale = target / height;
    let visual_scale = -2.0 * feet_offset * mesh_scale;
    HumanoidPlacement { mesh_scale, visual_scale }
}

/// True when the archetype's GLB is a converted EQ **world prop**, authored directly in EQ world
/// units with its own origin preserved — not a character rig that the renderer normalizes to a
/// character height.
///
/// `"boat"` is the only one today. Measured from the baked `boat.glb` (2026-07-27): a single glTF
/// node carrying no `scale`/`rotation`/`translation`, one mesh named `row.mod`, `POSITION` bounds
/// `x [-14.8816, 22.8514] y [-3.9823, 5.9629] z [-8.3050, 8.4137]` — i.e. raw EQ-unit vertices in
/// the source asset's own frame. `archetype_scale("boat")` is already `1.0` for exactly this
/// reason (#194: "the EQG model is already authored in EQ units").
///
/// There is no correct *constant* height for this class — a rowboat and a three-master share the
/// archetype — so the asset's own measured height is the target, which gives `mesh_scale == 1.0`.
/// That is what [`skinned_target_height`] returns, and it is why #756 added **no** `"boat"` arm to
/// [`archetype_target_height`]: any number written there would be invented rather than measured.
pub fn archetype_native_units(archetype: &str) -> bool {
    matches!(archetype, "boat")
}

/// [`target_height_for`] for the **skinned** model path, with the native-units exemption
/// ([`archetype_native_units`]) applied: a converted world prop renders at its authored size
/// (`target == true_height`, so `humanoid_placement` yields `mesh_scale == 1.0`) instead of being
/// normalized to a character height.
///
/// Latent today, deliberately (#756): every entity-archetype GLB in the shipped asset set except
/// `boat.glb` has a skin, so `boat` is the only archetype that reaches the static path and the
/// only one this exemption names — a skinned boat asset does not exist yet. Without this, a boat
/// that shipped with a skeleton would fall through `archetype_target_height`'s `_ => 6.0` and be
/// squashed to a 6-foot hull.
pub fn skinned_target_height(race: &str, archetype: &str, true_height: f32) -> f32 {
    if archetype_native_units(archetype) && true_height > 0.001 { true_height }
    else { target_height_for(race, archetype) }
}

/// Scale, vertical lift and horizontal recentre for placing a **static** (unskinned) entity model,
/// as the live render path uses them.
///
/// This is the single source of that math **for the render passes** — the entity pass, the player
/// pass, both static shadow-caster arms and the regression test all call it, so the test guards the
/// ACTUAL render placement rather than a hand-copied formula (the lesson [`HumanoidPlacement`]
/// records for the skinned path, #357). Before #756 the same three lines were written out inline at
/// four sites in `src/pass.rs`, and the `floating` exemption below was in none of them.
///
/// It is NOT the only copy in the repository, and saying otherwise would be false: the standalone
/// model-viewer binary (`src/bin/render_model.rs`, in the root `eqoxide` crate — it cannot depend on
/// this crate's pass module) still writes `2.0 * y_extent * arch_scale` (`render_model.rs:1097`
/// and `:1266`) and `vscale * 0.5 + y_bottom * arch_scale` (`:1271`) by hand. Consequence, stated
/// because it is real: the viewer has no `floating` concept at all and did not take #768's
/// correction either, so its static arm still lifts a model by `(y_extent + y_bottom) * arch_scale`
/// — the formula #768 replaced here. How far off that puts a given model on the turntable is not
/// stated, because the viewer's `arch_scale` comes from a CLI-supplied archetype name
/// (`render_model.rs:814`) and no run of the viewer was made for this change. Neither #756 nor #768
/// changed the viewer; converging it is its own change, and it is not a pure deletion there — the
/// viewer also feeds `visual_scale` to its camera distance (`:1098`), its camera focus height
/// (`:1291`) and its marker placement (`:1471`), so removing the term reframes the turntable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StaticPlacement {
    /// Uniform mesh scale fed to `entity_model_matrix_heading`'s `mesh_scale`.
    pub mesh_scale: f32,
    /// Horizontal recentre fed to `entity_model_matrix_heading`'s `center_xz`.
    pub center_xz: [f32; 2],
    /// The WHOLE vertical lift, fed to `entity_model_matrix_heading`'s `y_bottom` (scaled by
    /// `mesh_scale` there). `0.0` on the floating arm.
    ///
    /// There is deliberately no `visual_scale` field. `entity_model_matrix_heading` lifts by
    /// `visual_scale * 0.5 + y_bottom * mesh_scale`, and the static call sites pass a literal `0.0`
    /// for `visual_scale`, so this field is the only lift a static *placement* can express. #768 was
    /// exactly a second lift term hiding in `visual_scale`.
    ///
    /// Dropping the field removes that term from this shared helper. It does NOT make the term
    /// unwritable, and there are TWO ways back to it, not one — a caller can hand
    /// `2.0 * model.y_extent * p.mesh_scale` to `entity_model_matrix_heading` (nothing here
    /// constrains that argument), and a caller can hand `model.y_bottom + model.y_extent` to this
    /// function's `y_bottom` (nothing here constrains that either — an `f32` is an `f32`). Both were
    /// measured to leave every behavioural test in this crate green.
    ///
    /// What holds them today is a source-text pin over `pass.rs`,
    /// `tests/floating_placement.rs::every_static_placement_in_pass_rs_is_written_exactly_as_reviewed`,
    /// which requires each of the four call sites to be spelled exactly as reviewed. That bounds the
    /// four call sites in that one file. It is NOT a type-level guarantee and it does not reach a
    /// caller in another file; making the state unrepresentable would mean this function taking the
    /// model's measured bounds as an opaque value only the loader can mint, which is a wider change
    /// than #768 and is not done here.
    pub y_bottom: f32,
}

/// Compute the static-model placement from the model's measured bounds and the archetype scale.
///
/// `floating` is a REQUIRED argument rather than a default because it selects between two
/// different meanings of the entity's stored **z** (#756):
///
/// - **grounded** (`false`) — the stored z is eqoxide's FOOT datum: the ingest path subtracted
///   `WIRE_Z_OFFSET` (`crates/eqoxide-core/src/coord.rs:115` `wire_z_to_foot`). The model is lifted
///   so its **lowest vertex sits on that z**, which is the rule `GpuStaticModel::y_bottom` already
///   states (`gpu.rs:141-142`, "used to compute the ground lift so models stand at Z=0 instead of
///   floating or sinking").
///
///   The lift is `y_bottom * mesh_scale` and nothing else. Until #768 this arm ALSO passed
///   `visual_scale = 2 * y_extent * mesh_scale`, which `entity_model_matrix_heading` halves and adds
///   on top, so the real lift was `(y_extent + y_bottom) * mesh_scale` and the model's bottom landed
///   a full rendered model height above the stored z. Measured by mapping `boat.glb`'s bounding box
///   through this arm's production matrix (stored z 4, heading 0): drawn origin z was `17.927488`
///   with the lowest vertex at `13.945171` — `9.945171` too high, exactly `y_extent * mesh_scale`.
///   It is now `7.982317` / `4.000000`. The axis direction is not assumed:
///   `camera.rs:264` `static_model_y_up_axis_maps_to_world_up` pins that a static model's +Y
///   becomes world +Z, and `tests/floating_placement.rs` re-derives it from the matrix's own
///   columns before reading a lowest vertex off it.
///
///   Residual, stated because it is real and NOT fixed here: `y_bottom` is `-y_min` only when
///   `y_min < 0` and `0.0` otherwise (see the loader's reduction in this file). For a model whose
///   vertices all sit above its local origin this arm therefore lifts by `0` and the bottom lands
///   `y_min * mesh_scale` above the stored z. No shipped model exercises that clamp — `boat.glb` is
///   the only unskinned model `model_for` can load and its `y_min` is `-3.982317` — so #768 left the
///   clamp alone rather than change a datum the skinned path also reads.
///
///   Nothing rendered wrong because of the over-lift — true as measured on 2026-07-28, and stated
///   with the mechanism that actually decides it, because an earlier version of this paragraph got
///   that mechanism wrong. A model takes the static arm when `renderer::SkinFit::classify` returns
///   anything but `Fits` (`renderer.rs`, eqoxide#780 — before #780 this was the unnamed boolean
///   `!(0 < joint_count <= 128)`): no skin at all, **or** a skin with zero joints, **or** a skin
///   with MORE than `JOINT_CAP` (128) joints. "Unskinned" is sufficient, not necessary. Scanned
///   every name `model_for` can ask for (18 archetypes + 29 `race_*` + the 3 `_f` variants that
///   exist = 50 files): exactly one lands on the static arm, `boat.glb`, at `skins == 0`. Nothing
///   is over the 128 cap — re-measured independently for #780 by parsing the GLB JSON chunk of
///   every file in the local model cache directly (136 files, including zone terrain; the 51
///   character-relevant ones reproduce the same 50-file scan): 0 over cap, `race_pcfroglok.glb`
///   still the max at 127. `Entity::floating()` is `skips_wire_z_offset(is_boat, flymode)`
///   (`eqoxide-core/src/game_state.rs:192`), true for every boat regardless of flymode, so the
///   grounded arm has no live consumer today.
///
///   **The margin is one joint, and it is not ours to hold.** `race_pcfroglok.glb` has 127 joints
///   against the 128 cap; 11 rigs are at 109 or more. The model directory is an externally rebaked
///   sync target. A two-joint rebake of that one file would put a PC race on the grounded arm,
///   where `floating()` is false — which is why this arm has to be correct rather than merely
///   unreached. The absence of a live consumer is also why a false sentence could sit here
///   undetected, and why #768 has no live before/after to show. eqoxide#780 (this margin, filed by
///   the #773 reviewer) is fixed by giving that predicate a name (`SkinFit`) instead of a change of
///   behaviour: a model whose skin EXCEEDS the cap now logs loudly and is recorded in
///   `EqRenderer::skin_cap_downgrades`, but still renders through this same static arm.
/// - **floating** (`true`) — two separate steps, held to different standards:
///
///   1. *That the current lift is wrong* is certain from the code alone. `wire_z_to_foot`
///      (`coord.rs:115-117`) returns the wire z UNCHANGED for a floating entity, so a floating
///      spawn's stored z is by construction NOT in the foot datum. The grounding lift is a
///      foot-datum→placement conversion. Applying it to a z that was never converted is wrong
///      whatever the wire datum turns out to be.
///   2. *That the right lift is zero* is an INFERENCE, not a measurement. `coord.rs:8-9` states
///      the datum — "EQ's character `z` is the position of the **model origin**" — and
///      `coord.rs:34-35` records that boats skip the server's Z-offset entirely (`Mob::FixZ`
///      early-returns for them). Read together, a floating spawn's stored z is the position of
///      the model origin, so the origin goes there and the lift is zero. Note that `coord.rs:8-9`
///      is stated for *characters*; extending it to a boat hull is an inference from `coord.rs`,
///      not something measured against a running server. To be explicit about the gap: **no live
///      end-to-end run backs this.** #756 was verified by source-tracing, GLB measurement and unit
///      tests only — nobody has watched a hull placed this way sit on the water in play, because
///      the observable needs a boat-race spawn in a harbour zone. If it turns out the hull rides
///      high or low by a fixed amount, THIS is the line that was wrong; step (1) still holds.
///
///   Corroborating but NOT probative: `boat.glb`'s origin sits 3.9823u above the hull's lowest
///   vertex and 5.9629u below its highest, which is the shape you would expect of a rowboat
///   authored to be placed at its waterline. That is consistent with (2); it does not establish it.
///
/// Two things are deliberately NOT changed by the `floating` arm:
///
/// - **Scale.** `archetype_scale` applies on both arms: a floating spawn is placed differently,
///   not sized differently.
/// - **The horizontal recentre (`center_xz`).** The citations above are about **z** only. I did
///   not establish which datum the wire *xy* addresses — whether it is the model origin or the
///   mesh's xz centroid — so the recentre is passed through unchanged on both arms rather than
///   dropped on an assumption. For `boat.glb` the two differ by a measured
///   `center_xz = [3.9849, 0.0543]`, i.e. ~3.98u along the hull's own length axis (the recentre
///   is applied in model space, before the heading yaw, so it rotates with heading). That gap is
///   real and unresolved; it is not what #756 is about.
///
/// Magnitude of the grounded arm applied to a floating hull, from the measured `boat.glb` bounds
/// (`y_extent = 9.9452`, `y_bottom = 3.9823`, `archetype_scale("boat") = 1.0`): before #768 the two
/// arms differed by `2*9.9452*1.0*0.5 + 3.9823*1.0 = 13.9275` units of lift on a 9.9452-unit-tall
/// model; after it they differ by `3.9823`, the grounded arm's whole lift.
///
/// `y_extent` is no longer a parameter. It was only ever read to build the `visual_scale` term #768
/// removed, so keeping it would have left the over-lift one edit away from returning.
pub fn static_placement(
    archetype: &str, y_bottom: f32, center_xz: [f32; 2], floating: bool,
) -> StaticPlacement {
    let mesh_scale = archetype_scale(archetype);
    if floating {
        StaticPlacement { mesh_scale, center_xz, y_bottom: 0.0 }
    } else {
        StaticPlacement { mesh_scale, center_xz, y_bottom }
    }
}

/// Every per-race character model the asset server produces (`race_<code>.glb`,
/// one file per race+gender, gender encoded in the 3-letter code). Used at load
/// time to register the models that are present and log the ones that are not.
pub const PLAYABLE_RACE_MODELS: &[&str] = &[
    "race_hum", "race_huf", // Human
    "race_bam", "race_baf", // Barbarian
    "race_erm", "race_erf", // Erudite
    "race_elm", "race_elf", // Wood Elf
    "race_him", "race_hif", // High Elf
    "race_dam", "race_daf", // Dark Elf
    "race_ham", "race_haf", // Half Elf
    "race_dwm", "race_dwf", // Dwarf
    "race_trm", "race_trf", // Troll
    "race_ogm", "race_ogf", // Ogre
    "race_hom", "race_hof", // Halfling
    "race_gnm", "race_gnf", // Gnome
    "race_ikm", "race_ikf", // Iksar
    "race_kem", "race_kef", // Vah Shir
    "race_pcfroglok",       // Froglok (single archive, both genders)
];

/// The dedicated `race_<code>.glb` model basename for a playable race + gender,
/// or `None` for non-playable races (monsters) that render from an archetype
/// model. `gender`: 0 = male, 1 = female (2 = neuter falls through to male).
///
/// This is the **canonical** per-race mapping — there is NO fallback to a
/// look-alike race. A race whose model file is absent simply does not render
/// (the caller logs the missing model once). Codes are EQ's own model prefixes
/// from the Titanium client's `(race_id, gender)` table.
///
/// Keyed on the 3-letter race code from `eq_race_to_code`, where High Elf is
/// `"HIE"` (HIM/HIF models) and Half Elf is `"HEF"` (HAM/HAF models).
pub fn race_model_basename(race: &str, gender: u8) -> Option<&'static str> {
    let f = gender == 1;
    Some(match race.to_uppercase().as_str() {
        "HUM" => if f { "race_huf" } else { "race_hum" }, // Human
        "BAR" => if f { "race_baf" } else { "race_bam" }, // Barbarian
        "ERU" => if f { "race_erf" } else { "race_erm" }, // Erudite
        "ELF" => if f { "race_elf" } else { "race_elm" }, // Wood Elf
        "HIE" => if f { "race_hif" } else { "race_him" }, // High Elf
        "HEF" => if f { "race_haf" } else { "race_ham" }, // Half Elf
        "DKE" => if f { "race_daf" } else { "race_dam" }, // Dark Elf
        "DWF" => if f { "race_dwf" } else { "race_dwm" }, // Dwarf
        "TRL" => if f { "race_trf" } else { "race_trm" }, // Troll
        "OGR" => if f { "race_ogf" } else { "race_ogm" }, // Ogre
        "HFL" => if f { "race_hof" } else { "race_hom" }, // Halfling
        "GNM" => if f { "race_gnf" } else { "race_gnm" }, // Gnome
        "IKS" => if f { "race_ikf" } else { "race_ikm" }, // Iksar
        "VAH" => if f { "race_kef" } else { "race_kem" }, // Vah Shir
        "FRG" => "race_pcfroglok",                        // Froglok
        _ => return None,
    })
}

/// The character-model registry key `(key, gender_slot)` a spawn should render
/// with. Playable races resolve to their own `race_<code>` model with the gender
/// baked into the code (slot 0); everything else resolves to an archetype model
/// where the gender slot selects the female variant. There is no playable→archetype
/// fallback: a playable race with no loaded model yields a key that simply misses.
pub fn character_model_key(race: &str, gender: u8) -> (&'static str, u8) {
    match race_model_basename(race, gender) {
        Some(code) => (code, 0),
        None => (race_to_archetype(race), gender),
    }
}

/// IT model ids whose geometry is a shield, derived from the PEQ `items` table by
/// vote per idfile (`SUM(itemtype=8) > SUM(itemtype IN weapon-types)`) — stray
/// mislabeled rows share weapon idfiles (IT7 has one "shield" morningstar), and
/// misc items (potions) share shield idfiles (IT200), so neither a plain
/// `itemtype=8` set nor an all-rows majority is right. Sorted for binary search. IT200-228 are the classic
/// shield models; the rest are Luclin+ ranges.
const SHIELD_IT_IDS: &[u32] = &[
    48, 67, 200, 201, 202, 203, 204, 205, 206, 207, 208, 209,
    210, 211, 212, 213, 214, 215, 216, 217, 218, 219, 220, 221,
    222, 223, 224, 225, 226, 228, 10530, 10531, 10532, 10535, 10536, 10537,
    10538, 10540, 10542, 10543, 10544, 10611, 10645, 10646, 10664, 10665, 10668, 10669,
    10670, 10671, 10691, 10697, 10729, 10730, 10738, 10754, 10772, 10775, 10781, 10790,
    10826, 10827, 10832, 10833, 10843, 10849, 10850, 10857, 10858, 10960, 10961, 10963,
    10964, 10965, 10969, 10970, 10971, 10973, 10976, 10977, 10978, 10979, 10980, 10982,
    10983, 10984, 10985, 10986, 10987, 10988, 10989, 10990, 10991, 10992, 10993, 10994,
    10995, 10996, 11001, 11002, 11003, 11013, 11017, 11018, 11019, 11020, 11048, 11049,
    11085, 11086, 11102, 11103, 11110, 11111, 11142, 11143, 11144, 11145, 11183, 11185,
    11188, 11189, 11190, 11191, 11220, 11224, 11341, 11410, 11440, 11442, 11443, 11452,
    11460, 11469, 11478, 11486, 11490, 11491, 11492, 11493, 11494, 11495, 11496, 11497,
    11531, 11588, 11596, 11704, 11705, 11706, 11729, 11732, 11733, 11758, 11759, 11760,
    11783, 11786, 11787, 11797, 11872, 11873, 11874, 11875, 12143, 12144, 12145, 12146,
    12183, 12184, 12185, 12186, 12201, 12202, 12217, 12218, 12232, 12233, 12247, 12248,
    12390, 12391, 12400, 12406, 12412, 12428, 12447, 12455, 12461, 12467, 12483, 12573,
    12584, 12595, 12606, 12640, 12641, 12642, 12667, 12668, 12669, 12696, 12697, 12698,
    12723, 12724, 12749, 12750, 12751, 12776, 12777, 12778, 14000, 60123, 60135, 60136,
    60139, 60142, 60143, 60144, 60145, 60146, 60327, 60328, 60329, 60340, 67367, 67918,
    67919, 67939, 99251, 99252, 99253, 99276, 99277, 99278, 101025, 101037
];

/// Bone-local transform for drawing a held IT model at a rig attach bone.
///
/// weapons.glb vertices are NOT raw EQ coordinates: `bake_weapons_glb` routes IT
/// meshes through the zone pipeline, whose WLD reader swaps Y/Z — a det=-1
/// mirror `S`: (x,y,z) -> (x,z,y). The rig's `joint_world()` lives in the
/// converter's conjugated Y-up frame, a proper rotation `R` = -90° about X:
/// (x,y,z) -> (x,z,-y). The real client attaches IT actors with an identity
/// local transform in EQ space, so the draw needs `J·R·v_eq = J·(R·S)·v_glb`,
/// and `R·S = diag(1,1,-1)`: a bone-local Z negation.
///
/// The old code applied `R` here, treating the verts as raw EQ — that stacked a
/// rotation onto the already-mirrored bake and rendered every held item
/// reflected in the bone's local Z (shields faced backward and cut through the
/// arm, blades pointed the wrong way — eqoxide#178).
pub fn held_item_xform() -> glam::Mat4 {
    glam::Mat4::from_scale(glam::Vec3::new(1.0, 1.0, -1.0))
}

/// Whether an IT model id is a shield (drives the off-hand attach bone).
pub fn is_shield_it(it_id: u32) -> bool {
    SHIELD_IT_IDS.binary_search(&it_id).is_ok()
}

/// Attach bone for the player's secondary-hand item, from its IDFile string
/// ("IT210"): shields mount on the forearm SHIELD_POINT, everything else is
/// gripped at L_POINT (also the fallback for unparseable idfiles).
pub fn secondary_attach_bone(idfile: &str) -> &'static str {
    let it_id = idfile.trim().trim_start_matches(['I', 'T', 'i', 't']).parse::<u32>().unwrap_or(0);
    if is_shield_it(it_id) { "SHIELD_POINT" } else { "L_POINT" }
}

/// Held-item model keys + attach bones for a spawn's equipment array (wire slots:
/// primary=7, secondary=8). Spawn equipment carries the held model's numeric IT id
/// (`d_melee_texture_*` server-side); 0 = empty hand. Keys match the UPPERCASE
/// IDFile keys of `weapons.glb` ("IT7", "IT10649", …). Off-hand shields mount at
/// SHIELD_POINT; anything else is gripped. Dead entities show no held items — the
/// corpse pose has no meaningful hand attachment.
pub fn held_item_keys(equipment: &[u32; 9], dead: bool) -> [Option<(String, &'static str)>; 2] {
    if dead { return [None, None]; }
    let key = |n: u32, bone: &'static str| (n != 0).then(|| (format!("IT{n}"), bone));
    let sec_bone = if is_shield_it(equipment[8]) { "SHIELD_POINT" } else { "L_POINT" };
    [key(equipment[7], "R_POINT"), key(equipment[8], sec_bone)]
}

/// Numeric IT id from a held-item IDFile string ("IT10649" -> 10649). Empty / unparseable -> 0.
fn it_id_from_idfile(idfile: &str) -> u32 {
    idfile.trim().trim_start_matches(['I', 'T', 'i', 't']).parse::<u32>().unwrap_or(0)
}

/// Held-item model keys + attach bones for the **self player**, unified onto the SAME
/// server-authoritative source every other spawn uses: the equipment material array
/// (slots 7=primary, 8=secondary — the values the real client's `mob_appearance` derives
/// held models from, and the values eqoxide's billboard path already renders via
/// [`held_item_keys`]). The self player additionally has the inventory item's own IDFile
/// (worn slots 13/14); we prefer it when present so the working primary can't regress, and
/// fall back to the broadcast material when it is absent — an off-hand held item (e.g. a
/// cup/light-source) whose inventory item carries no IDFile still renders, matching the real
/// client, where it previously vanished for the self player only (eqoxide#515).
///
/// Primary → R_POINT (right hand), secondary → L_POINT (left hand) or SHIELD_POINT (shield):
/// the exact mapping [`held_item_keys`] applies, proven correct against the RoF2 skeleton
/// (do NOT swap the hands — #515's "wrong hand" half was a false report; the attach bones,
/// wire slots, and deployed asset were all verified correct).
pub fn self_held_item_keys(
    equipment: &[u32; 9], primary_idfile: &str, secondary_idfile: &str, dead: bool,
) -> [Option<(String, &'static str)>; 2] {
    let mut eff = *equipment;
    // IDFile takes precedence (non-empty); otherwise keep the broadcast material.
    let prim = it_id_from_idfile(primary_idfile);
    let sec  = it_id_from_idfile(secondary_idfile);
    if prim != 0 { eff[7] = prim; }
    if sec  != 0 { eff[8] = sec; }
    held_item_keys(&eff, dead)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── eqoxide#779: grade the loader's y_bottom/y_extent reduction ──────────────────────────
    //
    // #773's reviewer built the mutation `y_bottom = -y_min -> -y_min + (y_max - y_min)` (the
    // literal substring "-y_min" is only written once in `y_bottom_and_extent`, in the
    // `y_min < 0.0` branch) and ran it against the whole crate: 215 passed / 0 failed / 11
    // ignored, green. `(y_max - y_min)` is `y_extent`, so that mutation folds the model's whole
    // vertical extent into `y_bottom` — algebraically `y_bottom_correct + y_extent` — which is
    // exactly #768's over-lift (`StaticPlacement`'s lift is `y_bottom * mesh_scale`; #768 was a
    // second, separate `y_extent` term added on top of that same product). The two tests below
    // grade the reduction directly: one is a property over many generated bounds, the other is
    // the specific regression case with real measured data, both mutation-checked.

    #[test]
    fn y_bottom_and_extent_hold_the_spec_over_many_generated_bounds() {
        // A tiny xorshift PRNG so this is a genuine property test over MANY (y_min, y_max) pairs
        // rather than one hand-picked fixture, without adding a proptest/quickcheck dependency
        // this crate (and the rest of the workspace) does not otherwise have. Deterministic seed
        // -> reproducible on failure.
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next_u64 = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        // Map a u64 to a wide, finite f32 range so bounds routinely land off zero and off each
        // other: roughly [-1000.0, 1000.0] with fractional bits, not just small integers.
        let mut next_f32 = || {
            ((next_u64() as f64 / u64::MAX as f64) * 2000.0 - 1000.0) as f32
        };

        let mut saw_negative_y_min_with_extent = 0usize;

        for _ in 0..2000 {
            let a = next_f32();
            let b = next_f32();
            let (y_min, y_max) = if a <= b { (a, b) } else { (b, a) };

            let (y_bottom, y_extent) = y_bottom_and_extent(y_min, y_max);

            // Spec: y_bottom is -y_min clamped at zero from below.
            let expected_bottom = if y_min < 0.0 { -y_min } else { 0.0 };
            assert_eq!(
                y_bottom, expected_bottom,
                "y_bottom must be -y_min (clamped at 0), got {y_bottom} for y_min={y_min} y_max={y_max}"
            );

            // Spec: y_extent is the plain span.
            let expected_extent = y_max - y_min;
            assert_eq!(
                y_extent, expected_extent,
                "y_extent must be y_max - y_min, got {y_extent} for y_min={y_min} y_max={y_max}"
            );

            // THE property eqoxide#779 is about: y_bottom must not depend on y_max at all. Hold
            // y_min fixed, change y_max by an arbitrary nonzero amount, and confirm y_bottom is
            // unchanged. #779's corruption (and any equivalent mutation that folds y_extent, or
            // y_max directly, into y_bottom) makes y_bottom move when y_max moves; this is the
            // assertion that would catch that for every generated y_min, not just one fixture.
            let alt_y_max = y_max + 137.0;
            let (y_bottom_alt, _) = y_bottom_and_extent(y_min, alt_y_max);
            assert_eq!(
                y_bottom, y_bottom_alt,
                "y_bottom must be independent of y_max: y_min={y_min} y_max={y_max} vs alt_y_max={alt_y_max} \
                 gave y_bottom={y_bottom} vs {y_bottom_alt}"
            );

            if y_min < 0.0 && y_extent != 0.0 {
                saw_negative_y_min_with_extent += 1;
            }
        }

        // Guard against a silently degenerate run (e.g. a broken PRNG that only emits zeros,
        // or a range bug that never produces y_min < 0): without at least some samples where
        // y_min < 0.0 and y_extent != 0.0, the assertions above never actually exercise the
        // branch #779's corruption lives in, and this test would be worthless despite passing.
        assert!(
            saw_negative_y_min_with_extent > 100,
            "generated bounds never exercised the y_min<0 branch with nonzero extent \
             ({saw_negative_y_min_with_extent} of 2000) — this run could not have caught eqoxide#779's mutation"
        );
    }

    #[test]
    fn y_bottom_matches_the_intended_quantity_for_a_known_model() {
        // eqoxide#779: hand-known bounds for a real model, not a synthetic one — boat.glb's
        // measured Y bounds, the same constants `tests/floating_placement.rs` (BOAT_Y_MIN,
        // BOAT_Y_MAX) and this file's `static_placement` doc comment (`y_extent = 9.9452`) cite
        // as ground truth. y_min != 0, y_max != y_min (nonzero extent), so this pair cannot
        // collapse the correct and #779-corrupted formulas onto the same number — they coincide
        // only when y_extent == 0 (y_min == y_max), asserted explicitly below rather than assumed.
        let y_min = -3.982317_f32;
        let y_max = 5.962854_f32;

        let (y_bottom, y_extent) = y_bottom_and_extent(y_min, y_max);

        assert_eq!(y_bottom, 3.982317, "y_bottom must be -y_min, not fold in the model's extent");
        assert!((y_extent - 9.945171).abs() < 1e-4, "y_extent={y_extent}");

        // Sanity: confirm this fixture is NOT incidentally symmetric before trusting it as a
        // regression guard (eq-fixture-edits-are-not-local: a corpus edit once collapsed a
        // file's only mutant while everything stayed green). The correct and #779-corrupted
        // formulas coincide exactly when y_extent == 0; assert we are measurably off that.
        assert!(y_extent.abs() > 1.0, "fixture's extent is too close to 0 to distinguish the mutation");

        // eqoxide#779's exact corruption: "-y_min" (this file's `y_bottom_and_extent`, y_min<0.0
        // branch) becomes "-y_min + (y_max - y_min)". Applying that text substitution to this
        // fixture's own y_min/y_max must NOT equal the real y_bottom, or this fixture is useless
        // as a regression guard for it.
        let corrupted = -y_min + (y_max - y_min);
        assert_ne!(
            y_bottom, corrupted,
            "fixture collapsed: y_min={y_min} y_max={y_max} makes the correct and #779-corrupted \
             formulas equal, so this pair can't catch that mutation"
        );
        // And confirm the corrupted value is exactly #768's shape (y_bottom + y_extent), which is
        // the whole point of the issue: the same over-lift, reintroduced one file upstream.
        assert!((corrupted - (y_bottom + y_extent)).abs() < 1e-4);
    }

    #[test]
    fn held_item_keys_map_wire_slots_and_skip_dead() {
        let mut eq = [0u32; 9];
        eq[7] = 10649;
        eq[8] = 7;
        assert_eq!(held_item_keys(&eq, false),
                   [Some(("IT10649".into(), "R_POINT")), Some(("IT7".into(), "L_POINT"))]);
        assert_eq!(held_item_keys(&eq, true), [None, None]);
        assert_eq!(held_item_keys(&[0; 9], false), [None, None]);
    }

    #[test]
    fn self_held_prefers_idfile_and_falls_back_to_material() {
        // Primary maps to R_POINT, secondary to L_POINT — the SAME mapping as other spawns.
        // IDFile present → used verbatim.
        let eq = [0u32; 9];
        assert_eq!(
            self_held_item_keys(&eq, "IT10649", "IT7", false),
            [Some(("IT10649".into(), "R_POINT")), Some(("IT7".into(), "L_POINT"))],
            "primary -> right hand, secondary -> left hand"
        );

        // eqoxide#515: off-hand item with NO inventory IDFile but a broadcast material
        // (equipment[8]) now renders — previously the self path skipped it and the cup vanished.
        let mut eq = [0u32; 9];
        eq[7] = 10649; // dagger material broadcast
        eq[8] = 7;     // cup/off-hand material broadcast
        assert_eq!(
            self_held_item_keys(&eq, "", "", false),
            [Some(("IT10649".into(), "R_POINT")), Some(("IT7".into(), "L_POINT"))],
            "empty idfiles fall back to the server-authoritative equipment materials"
        );

        // IDFile precedence: a present IDFile wins over a (possibly stale) material.
        let mut eq = [0u32; 9];
        eq[7] = 999;
        assert_eq!(
            self_held_item_keys(&eq, "IT10649", "", false)[0],
            Some(("IT10649".into(), "R_POINT")),
            "inventory IDFile takes precedence over the broadcast material"
        );

        // A shield in the off hand routes to SHIELD_POINT whether it came from the idfile or material.
        let mut eq = [0u32; 9];
        eq[8] = 210; // shield material, no idfile
        assert_eq!(self_held_item_keys(&eq, "", "", false)[1],
                   Some(("IT210".into(), "SHIELD_POINT")));
        assert_eq!(self_held_item_keys(&[0; 9], "IT200", "IT210", false)[1],
                   Some(("IT210".into(), "SHIELD_POINT")));

        // Nothing equipped, or dead, draws no held items.
        assert_eq!(self_held_item_keys(&[0; 9], "", "", false), [None, None]);
        assert_eq!(self_held_item_keys(&eq, "IT10649", "IT7", true), [None, None]);
    }

    #[test]
    fn held_item_xform_bridges_baked_verts_to_an_identity_eq_attach() {
        // Contract: drawing a weapons.glb mesh with `J * held_item_xform()` must
        // equal attaching the raw EQ-space model at the bone with an identity
        // local transform, i.e. J * R * v_eq, where R is the converter's rig
        // bake rotation (-90° about X) and the baked vert is S·v_eq with S the
        // zone pipeline's Y/Z swap. Regression guard for eqoxide#178 (held items
        // rendered mirrored: shields faced backward, blades pointed wrong).
        let r = glam::Mat4::from_quat(
            glam::Quat::from_axis_angle(glam::Vec3::X, -std::f32::consts::FRAC_PI_2));
        let s = glam::Mat4::from_cols_array_2d(&[
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0], // Y column maps to Z
            [0.0, 1.0, 0.0, 0.0], // Z column maps to Y
            [0.0, 0.0, 0.0, 1.0],
        ]);
        // An arbitrary rigid bone pose, so the identity holds under any J.
        let j = glam::Mat4::from_rotation_translation(
            glam::Quat::from_euler(glam::EulerRot::XYZ, 0.3, -1.1, 2.0),
            glam::Vec3::new(-0.7, 1.4, 2.4));
        for v_eq in [glam::Vec3::X, glam::Vec3::Y, glam::Vec3::Z,
                     glam::Vec3::new(2.5, -0.3, 0.05)] {
            let want = j * r * v_eq.extend(1.0);           // identity attach in EQ space
            let baked = s * v_eq.extend(1.0);              // what weapons.glb stores
            let got = j * held_item_xform() * baked;       // what the draw computes
            assert!((want - got).length() < 1e-5, "v_eq={v_eq:?}: {want:?} != {got:?}");
        }
        // And the full mapping from authored EQ space is orientation-preserving
        // (det > 0) — the old `J * R * S` stack was a mirror.
        assert!((held_item_xform() * s).determinant() > 0.0);
    }

    #[test]
    fn shields_route_to_shield_point() {
        // IT200-228 are the classic shield models; IT7 (mace) and IT10649 (short
        // sword) are weapons even though stray mislabeled DB rows share the ids.
        assert!(is_shield_it(200));
        assert!(is_shield_it(228));
        assert!(is_shield_it(11085));
        assert!(!is_shield_it(7));
        assert!(!is_shield_it(10649));
        assert!(!is_shield_it(0));

        // A shield in the secondary slot attaches at SHIELD_POINT; primary never does
        // (even holding a shield model there — the real client only shield-mounts
        // the off hand).
        let mut eq = [0u32; 9];
        eq[7] = 200;
        eq[8] = 210;
        assert_eq!(held_item_keys(&eq, false),
                   [Some(("IT200".into(), "R_POINT")), Some(("IT210".into(), "SHIELD_POINT"))]);

        // Player-side idfile strings resolve the same way.
        assert_eq!(secondary_attach_bone("IT210"), "SHIELD_POINT");
        assert_eq!(secondary_attach_bone("IT7"), "L_POINT");
        assert_eq!(secondary_attach_bone("garbage"), "L_POINT");
    }

    #[test]
    fn load_returns_err_on_missing_file() {
        let result = ModelAsset::load(Path::new("/nonexistent/model.glb"));
        assert!(result.is_err());
    }

    #[test]
    #[ignore = "requires bundled model at eqoxide/assets/models/humanoid.glb"]
    fn load_humanoid_has_meshes() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        assert!(!asset.meshes.is_empty(), "expected at least one mesh");
    }

    #[test]
    #[ignore = "requires bundled model at eqoxide/assets/models/creature.glb"]
    fn load_creature_has_skin_and_clips() {
        let path = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/creature.glb")
        );
        let asset = ModelAsset::load(&path).expect("load failed");
        let skin = asset.skin.expect("creature.glb must have a skin");
        assert!(skin.joint_count > 0, "expected joints");
        assert!(skin.joint_count <= crate::renderer::JOINT_CAP,
            "too many joints for uniform buffer");
        assert!(!skin.clips.is_empty(), "expected animation clips");
        assert!(skin.clip_for_action("walking").is_some(), "no walking clip found");
    }

    #[test]
    #[ignore = "requires bundled model at eqoxide/assets/models/humanoid.glb"]
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
        assert!(skin.joint_count <= crate::renderer::JOINT_CAP,
            "joint count {} exceeds shader limit", skin.joint_count);
        let idx = skin.clip_for_action("walking")
            .expect("no walk clip found; clip names may not contain 'walk'");
        let clip = &skin.clips[idx];
        assert!(clip.duration > 0.0, "walk clip has zero duration");
        assert!(!clip.channels.is_empty(), "walk clip has no channels");
    }

    #[test]
    #[ignore = "requires bundled model at eqoxide/assets/models/humanoid.glb"]
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
    #[ignore = "requires bundled model at eqoxide/assets/models/humanoid.glb"]
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

    #[test]
    fn fish_gets_orientation_correction_others_identity() {
        // The substitute fish.glb renders mouth-down without a correction; only "fish" gets one (#149).
        assert_ne!(archetype_correction("fish"), glam::Mat4::IDENTITY, "fish must be re-oriented");
        for a in ["humanoid", "rat", "snake", "wolf", "bear", "worm", "creature"] {
            assert_eq!(archetype_correction(a), glam::Mat4::IDENTITY, "{a} must not be rotated");
        }
        // After the standard conversion the fish's mouth points world −Z (mouth-down); the
        // correction must send that to +X (the model-front the heading yaw points).
        let m = archetype_correction("fish");
        let nose = m.transform_vector3(-glam::Vec3::Z);
        assert!((nose.x - 1.0).abs() < 1e-5 && nose.y.abs() < 1e-5 && nose.z.abs() < 1e-5,
            "fish correction should send the −Z mouth to +X (front), got {nose:?}");
    }

    /// End-to-end: EQ race id → archetype model. Guards the run-10 fixes to the NPC race
    /// table (Skeleton/Zombie/Wasp/Rat/Gnoll/Fish/Kobold were mapped to wrong creatures).
    #[test]
    fn npc_race_ids_map_to_sensible_archetypes() {
        use eqoxide_core::race_class::eq_race_to_code;
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
        // Heights are raw EQ feet (world units == zone units). A human is 6 ft.
        assert!((archetype_target_height("humanoid") - 6.0).abs() < 0.01);
        assert!(archetype_target_height("dwarf") < archetype_target_height("humanoid"));
        assert!(archetype_target_height("unknown") > 0.0);
    }

    #[test]
    fn race_heights_match_eqemu_table() {
        // Raw EQ feet from GetRaceGenderDefaultHeight — a 6 ft human fits doorways.
        assert_eq!(race_target_height("HUM"), Some(6.0));
        // the reported discrepancy: wood/dark elves are 5/6 of a human, half elf 5.5
        assert_eq!(race_target_height("ELF"), Some(5.0)); // wood elf
        assert_eq!(race_target_height("DKE"), Some(5.0)); // dark elf
        assert_eq!(race_target_height("HIE"), Some(6.0)); // high elf
        assert_eq!(race_target_height("HEF"), Some(5.5)); // half elf
        // extremes
        assert_eq!(race_target_height("OGR"), Some(9.0));
        assert_eq!(race_target_height("GNM"), Some(3.0));
        // monsters are not in the playable table
        assert_eq!(race_target_height("GNL"), None);
        assert_eq!(race_target_height("RAT"), None);
    }

    #[test]
    fn race_heights_are_case_insensitive() {
        assert_eq!(race_target_height("elf"), race_target_height("ELF"));
        assert_eq!(race_target_height("Hum"), Some(6.0));
    }

    #[test]
    fn target_height_for_prefers_race_then_archetype() {
        // wood elf uses its race height (5.0 ft), not the "elf" archetype's 6.0
        assert_eq!(target_height_for("ELF", "elf"), 5.0);
        // a monster (gnoll) falls back to the archetype height
        assert_eq!(target_height_for("GNL", "gnoll"), archetype_target_height("gnoll"));
    }

    #[test]
    fn race_model_basename_maps_gender_and_race() {
        assert_eq!(race_model_basename("HUM", 0), Some("race_hum"));
        assert_eq!(race_model_basename("HUM", 1), Some("race_huf"));
        assert_eq!(race_model_basename("ELF", 0), Some("race_elm")); // wood elf male
        assert_eq!(race_model_basename("ELF", 1), Some("race_elf")); // wood elf female
        assert_eq!(race_model_basename("OGR", 0), Some("race_ogm"));
        assert_eq!(race_model_basename("HFL", 1), Some("race_hof")); // halfling female = HOF
        assert_eq!(race_model_basename("VAH", 0), Some("race_kem")); // vah shir = KEM/KEF
        // High Elf and Half Elf are distinct models, not collapsed
        assert_eq!(race_model_basename("HIE", 0), Some("race_him")); // high elf male
        assert_eq!(race_model_basename("HEF", 1), Some("race_haf")); // half elf female
        assert_ne!(race_model_basename("HIE", 0), race_model_basename("HEF", 0));
        assert_eq!(race_model_basename("FRG", 0), Some("race_pcfroglok"));
        // neuter (2) renders as male
        assert_eq!(race_model_basename("HUM", 2), Some("race_hum"));
        // monsters have no dedicated playable model
        assert_eq!(race_model_basename("GNL", 0), None);
        assert_eq!(race_model_basename("RAT", 0), None);
    }

    #[test]
    fn every_basename_is_a_registered_model() {
        // Anything race_model_basename can return must be in the load list, or it
        // would map to a key that is never loaded.
        for race in ["HUM", "BAR", "ERU", "ELF", "HIE", "HEF", "DKE", "DWF",
                     "TRL", "OGR", "HFL", "GNM", "IKS", "VAH", "FRG"] {
            for g in 0..=1 {
                let code = race_model_basename(race, g).unwrap();
                assert!(PLAYABLE_RACE_MODELS.contains(&code),
                    "{race} gender {g} -> {code} not in PLAYABLE_RACE_MODELS");
            }
        }
    }

    #[test]
    fn character_model_key_playable_vs_monster() {
        // playable race -> its own model, gender baked into the code (slot 0)
        assert_eq!(character_model_key("ELF", 1), ("race_elf", 0));
        assert_eq!(character_model_key("OGR", 0), ("race_ogm", 0));
        // monster -> archetype key, gender slot preserved for the female variant
        assert_eq!(character_model_key("GNL", 1), ("gnoll", 1));
    }

    /// Deterministic check of the player-pass placement math: load the real human
    /// model, replicate the LIVE skinned-player transform (src/pass.rs), and assert the
    /// rendered model ends up grounded (feet ≈ pos.z), horizontally centered on pos, and
    /// ~target tall.
    ///
    /// This test mirrors `src/pass.rs` exactly and MUST measure the bounds over
    /// triangle-referenced (indexed) vertices only. The glTF POSITION accessor is a
    /// SHARED vertex pool — gltf-rs `read_positions` returns the full pool for every
    /// primitive, so `mesh.positions` contains thousands of vertices this primitive's
    /// triangles never rasterize (see the "GLB shared vertex pool" note / #216). A raw
    /// max-min over ALL positions spans those unused strays and reports ≈12.6 for the
    /// human — double the real body — even though the rendered triangles span exactly the
    /// 6.0 target. (#357: an earlier version of this test used bind-pose grounding and a
    /// raw all-positions max-min, so it asserted `height 12.57 vs target 6.00` and failed;
    /// the model was never oversized — the render pipeline scales the robust idle extent
    /// (`true_height`) to target and grounds on the robust `feet_offset`.)
    #[test]
    fn humanoid_player_transform_grounds_and_centers() {
        let p = std::path::PathBuf::from(
            concat!(env!("CARGO_MANIFEST_DIR"), "/assets/models/humanoid.glb"));
        // assets/models/*.glb are bundled (gitignored) and absent in CI / fresh clones —
        // skip rather than fail when the model isn't present (matches
        // gendered_models_idle_ground_and_center).
        if !p.exists() { return; }
        let a = ModelAsset::load(&p).expect("load");
        let sk = a.skin.as_ref().expect("skin");
        let target = archetype_target_height("humanoid");
        // Derive scale + grounding from the SAME function the live renderer (src/pass.rs)
        // calls — not a hand copy — so this test genuinely guards the production render
        // scale. A 2× regression injected into humanoid_placement turns this assertion red.
        let placement = humanoid_placement(a.true_height, a.feet_offset, target);
        let pos = [100.0_f32, -200.0, 5.0];
        let mat = crate::camera::entity_model_matrix_heading(
            pos, 0.0, placement.visual_scale, placement.mesh_scale, [0.0, 0.0], true, 0.0,
            archetype_correction("humanoid"));
        let m = glam::Mat4::from_cols_array_2d(&mat);
        // Pose the model the way the live player renders it — the idle animation clip.
        let idle = sk.clip_for_action("idle").or_else(|| sk.clip_for_action("walking")).unwrap_or(0);
        let imats: Vec<glam::Mat4> = sk.evaluate(idle, 0.0).iter()
            .map(|x| glam::Mat4::from_cols_array_2d(x)).collect();
        let (mut mnx, mut mxx) = (f32::MAX, f32::MIN);
        let (mut mny, mut mxy) = (f32::MAX, f32::MIN);
        let (mut mnz, mut mxz) = (f32::MAX, f32::MIN);
        for (mesh, sdo) in a.meshes.iter().zip(a.skin_meshes.iter()) {
            if let Some(sd) = sdo {
                // Only triangle-referenced vertices — the geometry that actually renders.
                for &idx in &mesh.indices {
                    let vi = idx as usize;
                    if vi >= mesh.positions.len() { continue; }
                    let local = crate::anim::SkinData::skin_point(
                        mesh.positions[vi], sd.joint_indices[vi], sd.joint_weights[vi], &imats);
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
        let mat = crate::camera::entity_model_matrix_heading(pos, 0.0, visual_scale, ms, [cx, cz], true, 0.0, glam::Mat4::IDENTITY);
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
            let target = archetype_target_height(archetype);
            // Mirror the live renderer (src/pass.rs) via the shared placement fn — NOT a hand
            // copy — and measure over triangle-referenced (indexed) verts only, since the glTF
            // POSITION accessor is a shared pool of mostly-unused verts (#357).
            let placement = humanoid_placement(a.true_height, a.feet_offset, target);
            let mat = crate::camera::entity_model_matrix_heading(
                pos, 0.0, placement.visual_scale, placement.mesh_scale, [0.0, 0.0], true, 0.0,
                archetype_correction(archetype));
            let m = glam::Mat4::from_cols_array_2d(&mat);
            let imats: Vec<glam::Mat4> = sk.evaluate(idle, 0.0).iter()
                .map(|x| glam::Mat4::from_cols_array_2d(x)).collect();
            let (mut mnx, mut mxx, mut mny, mut mxy, mut mnz) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN, f32::MAX);
            for (mesh, sdo) in a.meshes.iter().zip(a.skin_meshes.iter()) {
                if let Some(sd) = sdo {
                    for &idx in &mesh.indices {
                        let vi = idx as usize;
                        if vi >= mesh.positions.len() { continue; }
                        let local = crate::anim::SkinData::skin_point(mesh.positions[vi], sd.joint_indices[vi], sd.joint_weights[vi], &imats);
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

    // ── head_part_visible truth table ───────────────────────────────────────

    #[test]
    fn head_part_visible_untagged_always_visible() {
        // body/eye/ear/fixed-head primitives have no tag → always visible
        assert!(head_part_visible(None, false, 0, 0));
        assert!(head_part_visible(None, true,  7, 7));
        assert!(head_part_visible(None, false, 3, 5));
    }

    #[test]
    fn head_part_visible_face_zero_shows_only_base() {
        // default face 0 → only the F=0 variants render
        assert!( head_part_visible(Some(HeadPart::Face(0)), false, 0, 0));
        for f in 1u8..=7 {
            assert!(!head_part_visible(Some(HeadPart::Face(f)), true, 0, 0),
                "Face({f}) should be hidden when face=0");
        }
    }

    #[test]
    fn head_part_visible_face_n_shows_only_n() {
        // face=3 → only the F=3 variants render; base and others hidden
        assert!( head_part_visible(Some(HeadPart::Face(3)), true, 3, 0));
        assert!( head_part_visible(Some(HeadPart::Hair(Some(3))), true, 3, 0));
        for f in [0u8, 1, 2, 4, 7] {
            assert!(!head_part_visible(Some(HeadPart::Face(f)), false, 3, 0),
                "Face({f}) should be hidden when face=3");
            assert!(!head_part_visible(Some(HeadPart::Hair(Some(f))), false, 3, 0),
                "Hair(Some({f})) should be hidden when face=3");
        }
    }

    #[test]
    fn head_part_visible_ignores_hairstyle() {
        // hairstyle selects nothing: RoF2 ships no hairstyle geometry/textures for
        // S3D player races (the client's actor-attach path finds no actor).
        assert!(head_part_visible(Some(HeadPart::Face(2)), false, 2, 5));
        assert!(head_part_visible(Some(HeadPart::Face(2)), false, 2, 0));
        assert!(head_part_visible(Some(HeadPart::Hair(None)), false, 2, 5));
    }

    #[test]
    fn crown_hair_always_visible() {
        // fixed crown-strip hair (no face variant) renders for every face value
        for f in 0u8..=7 {
            assert!(head_part_visible(Some(HeadPart::Hair(None)), false, f, 0));
        }
    }

    #[test]
    fn head_part_visible_default_hidden_flag_ignored_when_face_matches() {
        // default_hidden=true on the F=0 variant, but face=0 matches → visible anyway
        assert!(head_part_visible(Some(HeadPart::Face(0)), true, 0, 0));
    }

    // ── face-variant + painted-hair contract (asset-server hair fix) ────────────────────────

    #[test]
    fn parse_head_extras_classifies_hair_vs_face_vs_none() {
        use serde_json::json;
        // eq_face + eq_head_part=="hair" → tinted scalp half of face variant F.
        assert_eq!(
            parse_head_extras(&json!({"eq_head_part":"hair","eq_face":3,"eq_default_hidden":true})),
            Some((HeadPart::Hair(Some(3)), true)));
        // plain eq_face → facial skin variant, untinted.
        assert_eq!(
            parse_head_extras(&json!({"eq_face":2})),
            Some((HeadPart::Face(2), false)));
        // eq_head_part=="hair" alone → always-visible fixed crown hair.
        assert_eq!(
            parse_head_extras(&json!({"eq_head_part":"hair"})),
            Some((HeadPart::Hair(None), false)));
        // legacy pre-fix GLBs used eq_hairstyle for the same variants → read as face.
        assert_eq!(
            parse_head_extras(&json!({"eq_hairstyle":2})),
            Some((HeadPart::Face(2), false)));
        assert_eq!(
            parse_head_extras(&json!({"eq_head_part":"hair","eq_hairstyle":4,"eq_default_hidden":true})),
            Some((HeadPart::Hair(Some(4)), true)));
        // no tags → untagged (body/eyes/fixed skin) → None.
        assert_eq!(parse_head_extras(&json!({"foo":1})), None);
    }

    #[test]
    fn only_hair_parts_are_tinted_by_haircolor() {
        // Tint-eligible race (Dark Elf): painted-hair scalp → hair_tint(haircolor);
        // index 0 = [46,26,12].
        assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(1))), 0, "DKE", 0),
            Some([46.0/255.0, 26.0/255.0, 12.0/255.0, 1.0]));
        assert_eq!(head_part_tint(Some(HeadPart::Hair(None)), 0, "DKE", 0),
            Some([46.0/255.0, 26.0/255.0, 12.0/255.0, 1.0]));
        // haircolor >= 24 → white (no visible tint) but still Some for hair.
        assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(1))), 24, "DKE", 0),
            Some([1.0, 1.0, 1.0, 1.0]));
        // facial skin + untagged prims are never tinted.
        assert_eq!(head_part_tint(Some(HeadPart::Face(1)), 0, "DKE", 0), None);
        assert_eq!(head_part_tint(None, 0, "DKE", 0), None);
    }

    /// #519 raccoon-mask regression guard: the native RoF2 client never tints HUM hair
    /// (or any race outside male HIE/DKE/HEF + female DWF). A HUM male hair prim with a dark
    /// haircolor must render WHITE (untinted) — multiplying the skin-toned scalp/eye-band
    /// texels by hair_tint(0) ≈ near-black is exactly what painted the dark band across
    /// the eyes and the black scalp cap.
    #[test]
    fn hum_hair_is_never_tinted_native_gate() {
        for hc in [0u8, 1, 18, 23] {
            assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(0))), hc, "HUM", 0),
                Some([1.0, 1.0, 1.0, 1.0]),
                "HUM male hair must be untinted for haircolor {hc}");
            assert_eq!(head_part_tint(Some(HeadPart::Hair(None)), hc, "HUM", 1),
                Some([1.0, 1.0, 1.0, 1.0]),
                "HUM female fixed-crown hair must be untinted for haircolor {hc}");
        }
        // Dwarf: only the FEMALE model is tinted in the native client.
        assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(0))), 0, "DWF", 0),
            Some([1.0, 1.0, 1.0, 1.0]));
        assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(0))), 0, "DWF", 1),
            Some([46.0/255.0, 26.0/255.0, 12.0/255.0, 1.0]));
        // Elves: only the MALE model is tinted in the native client. A tint on the
        // FEMALE model would relocate #519's raccoon-mask bug onto her instead of
        // fixing it (review follow-up on PR #524).
        assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(0))), 0, "DKE", 1),
            Some([1.0, 1.0, 1.0, 1.0]),
            "female DKE hair must be untinted");
        // Untinted races still return Some(white) — hair prims must not fall back to an
        // equipment tint.
        assert_eq!(head_part_tint(Some(HeadPart::Hair(Some(2))), 5, "BAR", 0),
            Some([1.0, 1.0, 1.0, 1.0]));
    }
}
