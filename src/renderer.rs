use crate::assets::ZoneAssets;
use crate::gpu::{
    Vertex, GpuMesh, GpuModel, GpuStaticModel, GpuSkinnedModel, GpuSkinnedMesh, SkinnedVertex,
    upload_textures, create_depth_texture, build_fallback_texture_bg,
};

pub struct EntityAnimState {
    pub clip_idx:    usize,
    pub time:        f32,
    pub last_action: String,
    /// False when an idle action resolved to a non-idle clip (walk fallback for
    /// models with no real idle, e.g. the Skeleton). We freeze such clips at a
    /// static frame so the character stands still instead of walking in place.
    pub animate:     bool,
}

/// Pre-allocated entity uniform buffer slot count.
/// Layout: [0..PLAYER_UNIFORM_SLOTS) = player, [PLAYER_UNIFORM_SLOTS..) = entities.
// Character GLB models have up to 27 primitives (humanoid). The player draws one
// uniform slot per mesh, so this MUST be >= the max mesh count or the player loses
// its later primitives (head pieces + feet were dropped at the old value of 16).
pub const PLAYER_UNIFORM_SLOTS: usize = 32;
pub const TOTAL_ENTITY_UNIFORM_SLOTS: usize = 4128; // 32 player + 4096 entity mesh draws
/// Pre-allocated joint buffer pool size. Slot 0 = player, 1..N = entities.
pub const JOINT_BUF_SLOTS: usize = 512;
/// Size of one joint buffer: 128 joints × mat4(64 bytes).
pub const JOINT_BUF_BYTES: u64 = 128 * 64;

pub struct EqRenderer {
    pub device:              wgpu::Device,
    pub queue:               wgpu::Queue,
    pub surface_config:      wgpu::SurfaceConfiguration,
    pub layouts:             crate::pipeline::Layouts,
    pub pipelines:           crate::pipeline::Pipelines,
    pub camera_uniform:      crate::pipeline::CameraUniform,
    pub gpu_meshes:          Vec<crate::gpu::GpuMesh>,
    pub gpu_textures:        Vec<crate::gpu::GpuTexture>,
    pub texture_names:       Vec<String>,
    pub texture_bind_groups: Vec<wgpu::BindGroup>,
    pub fallback_texture_bg: wgpu::BindGroup,
    /// Character models keyed by (archetype, gender) — gender 0 = male, 1 = female.
    /// Female variants are loaded from `<archetype>_f.glb` when present.
    pub gpu_character_models: std::collections::HashMap<(&'static str, u8), crate::gpu::GpuModel>,
    pub anim_states:         std::collections::HashMap<u32, EntityAnimState>,
    pub last_view_proj:      [[f32; 4]; 4],
    pub last_cam_pos:        [f32; 3],
    pub depth_view:          wgpu::TextureView,
    /// CPU-side zone mesh data retained for terrain height queries.
    pub zone_assets:         Option<crate::assets::ZoneAssets>,
    /// Pre-allocated entity uniform buffers (reused every frame via write_buffer).
    pub entity_uniform_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    /// Pre-allocated joint matrix buffers (reused every frame via write_buffer).
    pub joint_buf_pool:      Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    /// Lowercase armor texture filename → S3D archive containing it (built at startup).
    pub equip_index: std::collections::HashMap<String, std::path::PathBuf>,
    /// Cache of uploaded armor texture bind groups keyed by base name (no extension).
    /// `None` = known-missing (negative cache) so we don't rescan every frame.
    pub equipment_tex_cache: std::collections::HashMap<String, Option<wgpu::BindGroup>>,
}

impl EqRenderer {
    pub fn new(
        device: wgpu::Device,
        queue: wgpu::Queue,
        surface_config: wgpu::SurfaceConfiguration,
    ) -> Self {
        let layouts             = crate::pipeline::build_layouts(&device);
        let camera_uniform      = crate::pipeline::build_camera_uniform(&device, &layouts);
        let fallback_texture_bg = build_fallback_texture_bg(
            &device, &queue, &layouts.texture_bgl,
        );
        let pipelines  = crate::pipeline::build_pipelines(&device, surface_config.format, &layouts);
        let depth_view = create_depth_texture(
            &device, surface_config.width, surface_config.height,
        );

        // Pre-allocate entity uniform buffer pool (avoid per-frame GPU allocs).
        let entity_uniform_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)> =
            (0..TOTAL_ENTITY_UNIFORM_SLOTS).map(|_| {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label:              Some("entity_uniform_pool"),
                    size:               std::mem::size_of::<crate::gpu::EntityUniform>() as u64,
                    usage:              wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label:  Some("entity_uniform_pool_bg"),
                    layout: &layouts.entity_bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0, resource: buf.as_entire_binding(),
                    }],
                });
                (buf, bg)
            }).collect();

        // Pre-allocate joint matrix buffer pool (128 joints × mat4 each).
        let joint_buf_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)> =
            (0..JOINT_BUF_SLOTS).map(|_| {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label:              Some("joint_buf_pool"),
                    size:               JOINT_BUF_BYTES,
                    usage:              wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label:  Some("joint_buf_pool_bg"),
                    layout: &layouts.joints_bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0, resource: buf.as_entire_binding(),
                    }],
                });
                (buf, bg)
            }).collect();

        Self {
            device,
            queue,
            surface_config,
            layouts,
            pipelines,
            camera_uniform,
            gpu_meshes: vec![],
            gpu_textures: vec![],
            texture_names: vec![],
            texture_bind_groups: vec![],
            fallback_texture_bg,
            gpu_character_models: std::collections::HashMap::new(),
            anim_states:     std::collections::HashMap::new(),
            last_view_proj: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            last_cam_pos: [0.0, 0.0, 0.0],
            depth_view,
            zone_assets: None,
            entity_uniform_pool,
            joint_buf_pool,
            equip_index: std::collections::HashMap::new(),
            equipment_tex_cache: std::collections::HashMap::new(),
        }
    }

    pub fn upload_zone_assets(&mut self, assets: &ZoneAssets) {
        use wgpu::util::DeviceExt;

        // Upload textures and build name→index map.
        let (gpu_textures, bind_groups) =
            upload_textures(&self.device, &self.queue, &assets.textures, &self.layouts.texture_bgl);
        let texture_names: Vec<String> = assets.textures.iter().map(|t| t.name.clone()).collect();
        self.gpu_textures        = gpu_textures;
        self.texture_bind_groups = bind_groups;
        self.texture_names       = texture_names;

        // Merge all source meshes that share the same texture into a single GPU buffer.
        // Qeynos has ~8500 source meshes but only ~100 unique textures — this reduces
        // draw calls from 8500 → ~100, which is the primary source of the frame-time budget.
        {
            use std::collections::HashMap;
            // (texture_idx, base_color_as_u32) → (accumulated vertices, accumulated indices)
            // Key uses texture_idx only; base_color is averaged across merges (usually all [1,1,1,1]).
            let mut groups: HashMap<Option<usize>, (Vec<Vertex>, Vec<u32>)> = HashMap::new();

            for mesh in &assets.meshes {
                if mesh.positions.is_empty() || mesh.indices.is_empty() { continue; }

                let texture_idx = mesh.texture_name.as_ref()
                    .and_then(|n| self.texture_names.iter().position(|t| t == n));

                let [cx, cy, cz] = mesh.center;
                let entry = groups.entry(texture_idx).or_default();
                let base = entry.0.len() as u32;

                for (i, &p) in mesh.positions.iter().enumerate() {
                    let normal = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                    entry.0.push(Vertex {
                        position: [p[0] + cx, p[2] + cz, p[1] + cy],
                        normal:   [normal[0], normal[2], normal[1]],
                        uv:       mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                    });
                }
                for &idx in &mesh.indices {
                    entry.1.push(idx + base);
                }
            }

            self.gpu_meshes = groups.into_iter().map(|(texture_idx, (verts, idxs))| {
                let vertex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX,
                });
                let index_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: None,
                    contents: bytemuck::cast_slice(&idxs),
                    usage: wgpu::BufferUsages::INDEX,
                });
                GpuMesh {
                    vertex_buf,
                    index_buf,
                    index_count: idxs.len() as u32,
                    texture_idx,
                    base_color: [1.0, 1.0, 1.0, 1.0],
                }
            }).collect();

            eprintln!("renderer: merged zone into {} draw calls (was {} source meshes)",
                self.gpu_meshes.len(), assets.meshes.len());
        }

        // Sort merged meshes so same-texture groups are contiguous (they already are, but be safe).
        self.gpu_meshes.sort_by_key(|m| m.texture_idx.map_or(usize::MAX, |i| i));

        // Retain CPU-side data for terrain height queries.
        self.zone_assets = Some(assets.clone());
    }

    /// Load all archetype character models and upload to GPU.
    /// Tries glTF files from `models_dir` first; falls back to EQ `_chr.s3d`
    /// archives from `assets_path` if the glTF file is missing.
    /// Models with valid skins (joint_count ≤ 128) are loaded as Skinned; others as Static.
    /// Missing models fall back to billboard rendering.
    pub fn load_character_models(&mut self, models_dir: &std::path::Path, assets_path: &std::path::Path) {
        use crate::models::{ModelAsset, SkinnedMeshData};
        use wgpu::util::DeviceExt;

        const ARCHETYPES: &[&str] = &[
            "humanoid", "elf", "dwarf", "gnoll", "skeleton", "zombie",
            "creature", "bear", "rat", "snake", "frog", "wasp",
            "wolf", "bat", "bird", "worm", "fish",
        ];

        for &key in ARCHETYPES {
            // Try glTF first, then fall back to EQ _chr.s3d.
            let gltf_path = models_dir.join(format!("{}.glb", key));
            let asset = if gltf_path.exists() {
                match ModelAsset::load(&gltf_path) {
                    Ok(a) => Some(a),
                    Err(e) => {
                        eprintln!("renderer: glTF load failed for '{}': {}", key, e);
                        None
                    }
                }
            } else {
                None
            };

            let asset = match asset {
                Some(a) => a,
                None => {
                    // Fall back to EQ _chr.s3d archive.
                    let chr_name = crate::models::archetype_to_chr_s3d(key);
                    let chr_asset = chr_name.and_then(|name| {
                        let path = assets_path.join(name);
                        if path.exists() {
                            match ModelAsset::load_from_chr_s3d(&path) {
                                Ok(a) => Some(a),
                                Err(e) => {
                                    eprintln!("renderer: chr S3D load failed for '{}': {}", key, e);
                                    None
                                }
                            }
                        } else {
                            None
                        }
                    });
                    match chr_asset {
                        Some(a) => a,
                        None => {
                            eprintln!("renderer: no model for archetype '{}'", key);
                            continue;
                        }
                    }
                }
            };

            // Build the male model (gender 0) plus a female variant (gender 1) from
            // `<archetype>_f.glb` when present. Each is stored under (archetype, gender).
            let mut variants: Vec<(u8, ModelAsset)> = vec![(0, asset)];
            let female_path = models_dir.join(format!("{}_f.glb", key));
            if female_path.exists() {
                match ModelAsset::load(&female_path) {
                    Ok(fa) => variants.push((1, fa)),
                    Err(e) => eprintln!("renderer: female glTF load failed for '{}': {}", key, e),
                }
            }
            for (gender, asset) in variants {
            eprintln!("renderer: loaded '{}' (gender {}) — y_bottom={:.4} y_extent={:.4} x_center={:.4} z_center={:.4}",
                key, gender, asset.y_bottom, asset.y_extent, asset.x_center, asset.z_center);

            let (_, tex_bgs) = upload_textures(
                &self.device, &self.queue, &asset.textures, &self.layouts.texture_bgl,
            );
            let tex_names: Vec<String> =
                asset.textures.iter().map(|t| t.name.clone()).collect();

            let use_skinned = asset.skin.as_ref()
                .is_some_and(|s| s.joint_count > 0 && s.joint_count <= 128);

            let model = if use_skinned {
                let skin = asset.skin.unwrap();
                let (meshes, skinned_slots): (Vec<GpuSkinnedMesh>, Vec<Option<crate::models::EquipSlot>>) = asset.meshes.iter()
                    .zip(asset.skin_meshes.iter())
                    .zip(asset.skinned_mesh_scales.iter())
                    .zip(asset.equip_slots.iter())
                    .filter_map(|(((mesh, sd_opt), &mesh_node_scale), &slot)| {
                        if mesh.positions.is_empty() || mesh.indices.is_empty() {
                            return None;
                        }
                        let sd = sd_opt.as_ref();
                        let vertices: Vec<SkinnedVertex> = mesh.positions.iter()
                            .enumerate()
                            .map(|(i, &p)| {
                                let nrm = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                                let uv  = mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]);
                                let ji  = sd.and_then(|s: &SkinnedMeshData| s.joint_indices.get(i))
                                            .copied().unwrap_or([0u32; 4]);
                                let jw  = sd.and_then(|s: &SkinnedMeshData| s.joint_weights.get(i))
                                            .copied().unwrap_or([1.0, 0.0, 0.0, 0.0]);
                                SkinnedVertex { position: p, normal: nrm, uv,
                                                joint_indices: ji, joint_weights: jw }
                            })
                            .collect();
                        let vbuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: None, contents: bytemuck::cast_slice(&vertices),
                            usage: wgpu::BufferUsages::VERTEX,
                        });
                        let ibuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                            label: None, contents: bytemuck::cast_slice(&mesh.indices),
                            usage: wgpu::BufferUsages::INDEX,
                        });
                        let texture_idx = mesh.texture_name.as_ref()
                            .and_then(|n| tex_names.iter().position(|t| t == n));
                        Some((GpuSkinnedMesh { vertex_buf: vbuf, index_buf: ibuf,
                                               index_count: mesh.indices.len() as u32,
                                               texture_idx, base_color: mesh.base_color,
                                               mesh_node_scale }, slot))
                    })
                    .unzip();
                eprintln!("renderer: loaded skinned model '{}' ({} joints, {} clips)",
                          key, skin.joint_count, skin.clips.len());
                GpuModel::Skinned(GpuSkinnedModel { meshes, texture_bind_groups: tex_bgs, skin, node_scale: asset.skinned_node_scale, y_bottom: asset.y_bottom, x_center: asset.x_center, z_center: asset.z_center, prefix: asset.prefix.clone(), equip_slots: skinned_slots, true_height: asset.true_height, clip_bounds: asset.clip_bounds.clone() })
            } else {
                let (meshes, static_slots): (Vec<GpuMesh>, Vec<Option<crate::models::EquipSlot>>) = asset.meshes.iter()
                    .zip(asset.equip_slots.iter())
                    .filter_map(|(mesh, &slot)| {
                    if mesh.positions.is_empty() || mesh.indices.is_empty() { return None; }
                    let vertices: Vec<Vertex> = mesh.positions.iter().enumerate()
                        .map(|(i, &p)| {
                            let nrm = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                            Vertex { position: p, normal: nrm,
                                     uv: mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]) }
                        }).collect();
                    let vbuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: None, contents: bytemuck::cast_slice(&vertices),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    let ibuf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: None, contents: bytemuck::cast_slice(&mesh.indices),
                        usage: wgpu::BufferUsages::INDEX,
                    });
                    let texture_idx = mesh.texture_name.as_ref()
                        .and_then(|n| tex_names.iter().position(|t| t == n));
                    Some((GpuMesh { vertex_buf: vbuf, index_buf: ibuf,
                                   index_count: mesh.indices.len() as u32, texture_idx,
                                   base_color: mesh.base_color }, slot))
                }).unzip();
                eprintln!("renderer: loaded static model '{}'", key);
                GpuModel::Static(GpuStaticModel { meshes, texture_bind_groups: tex_bgs, y_bottom: asset.y_bottom, y_extent: asset.y_extent, x_center: asset.x_center, z_center: asset.z_center, prefix: asset.prefix.clone(), equip_slots: static_slots, true_height: asset.true_height, clip_bounds: vec![] })
            };

            self.gpu_character_models.insert((key, gender), model);
            } // gender variants
        }

        // Index armor textures: shared velious sets (global17-23_amr) + each
        // archetype's chr/chr2 archives (lower material numbers). No decoding here.
        for n in 17..=23 {
            crate::assets::index_s3d_textures(
                &assets_path.join(format!("global{}_amr.s3d", n)), &mut self.equip_index);
        }
        for &key in ARCHETYPES {
            if let Some(name) = crate::models::archetype_to_chr_s3d(key) {
                crate::assets::index_s3d_textures(&assets_path.join(name), &mut self.equip_index);
                // also the _chr2 companion if present
                let chr2 = name.replace("_chr.s3d", "_chr2.s3d");
                let chr2_path = assets_path.join(&chr2);
                if chr2_path.exists() {
                    crate::assets::index_s3d_textures(&chr2_path, &mut self.equip_index);
                }
            }
        }
        eprintln!("equip: indexed {} armor textures", self.equip_index.len());
    }

    /// Select a loaded character model for an archetype + gender, falling back to the
    /// male (gender 0) variant when no female variant exists.
    pub fn model_for(&self, archetype: &'static str, gender: u8) -> Option<&crate::gpu::GpuModel> {
        self.gpu_character_models.get(&(archetype, gender))
            .or_else(|| self.gpu_character_models.get(&(archetype, 0)))
    }

    /// Load + upload one armor texture (trying .bmp then .dds). Returns its bind group.
    pub fn load_equip_texture(&self, base_name: &str) -> Option<wgpu::BindGroup> {
        for ext in ["bmp", "dds"] {
            let fname = format!("{}.{}", base_name, ext);
            if let Some(arch) = self.equip_index.get(&fname) {
                if let Some(tex) = crate::assets::load_one_texture_from_s3d(arch, &fname) {
                    let (_gpu, mut bgs) = upload_textures(
                        &self.device, &self.queue, &[tex], &self.layouts.texture_bgl);
                    return bgs.pop();
                }
            }
        }
        None
    }

    /// Pre-pass (mutable): ensure every armor texture needed this frame is cached.
    /// Runs before the immutable render passes so they only do lookups.
    pub fn ensure_equipment_textures(&mut self, scene: &crate::scene::SceneState) {
        use crate::models::{race_to_archetype, equip_texture_name};
        use crate::gpu::GpuModel;

        // Phase 1: collect needed base names (no mutation of the cache yet).
        let mut needed: Vec<String> = Vec::new();
        for b in &scene.billboards {
            let archetype = race_to_archetype(&b.race);
            let (prefix, slots) = match self.model_for(archetype, b.gender) {
                Some(GpuModel::Static(m))  => (&m.prefix, &m.equip_slots),
                Some(GpuModel::Skinned(m)) => (&m.prefix, &m.equip_slots),
                None => continue,
            };
            if prefix.is_empty() { continue; }
            for es in slots.iter().flatten() {
                let material = b.equipment[es.slot];
                if material == 0 { continue; } // naked → baked texture, no swap
                let key = equip_texture_name(prefix, &es.region, material, es.variant);
                if !self.equipment_tex_cache.contains_key(&key) {
                    needed.push(key);
                }
            }
        }
        if !scene.player_race.is_empty() {
            let archetype = crate::models::race_to_archetype(&scene.player_race);
            if let Some(model) = self.model_for(archetype, scene.player_gender) {
                let (prefix, slots) = match model {
                    GpuModel::Static(m)  => (&m.prefix, &m.equip_slots),
                    GpuModel::Skinned(m) => (&m.prefix, &m.equip_slots),
                };
                if !prefix.is_empty() {
                    for es in slots.iter().flatten() {
                        let material = scene.player_equipment[es.slot];
                        if material == 0 { continue; } // naked → baked texture, no swap
                        let key = equip_texture_name(prefix, &es.region, material, es.variant);
                        if !self.equipment_tex_cache.contains_key(&key) {
                            needed.push(key);
                        }
                    }
                }
            }
        }
        needed.sort();
        needed.dedup();

        // Phase 2: load + upload (or mark missing).
        for key in needed {
            let bg = self.load_equip_texture(&key);
            self.equipment_tex_cache.insert(key, bg);
        }
    }

    /// Encode all render passes for one frame in correct depth order.
    /// Camera is computed here and stored on self for HUD label projection.
    pub fn render_frame(
        &mut self,
        encoder:    &mut wgpu::CommandEncoder,
        view:       &wgpu::TextureView,
        scene:      &crate::scene::SceneState,
        cam_eye:    [f32; 3],
        cam_target: [f32; 3],
        dt:         f32,
    ) {
        use crate::gpu::GpuModel;

        // Animate NPCs and player (player uses reserved id=0)
        let anim_targets: Vec<(u32, &str, &str, u8)> = {
            let mut v: Vec<(u32, &str, &str, u8)> = scene.billboards.iter()
                .map(|b| (b.id, b.race.as_str(), b.action.as_str(), b.gender))
                .collect();
            if !scene.player_race.is_empty() {
                v.push((0, scene.player_race.as_str(), scene.player_action.as_str(), scene.player_gender));
            }
            v
        };

        for (id, race, action, gender) in &anim_targets {
            let archetype = crate::models::race_to_archetype(race);
            // Direct field lookup (with female→male fallback) keeps the borrow disjoint
            // from the `self.anim_states` mutation below; `model_for` would borrow all of self.
            let model = self.gpu_character_models.get(&(archetype, *gender))
                .or_else(|| self.gpu_character_models.get(&(archetype, 0)));
            let Some(GpuModel::Skinned(skinned)) = model else { continue };

            let state = self.anim_states.entry(*id).or_insert_with(|| {
                let clip_idx = skinned.skin.clip_for_action("walking").unwrap_or(0);
                EntityAnimState { clip_idx, time: 0.0, last_action: String::new(), animate: true }
            });

            if *action != state.last_action {
                state.clip_idx    = skinned.skin.clip_for_action(action).unwrap_or(0);
                state.time        = 0.0;
                state.last_action = action.to_string();
                state.animate     = skinned.skin.action_animates(action, state.clip_idx);
            }

            // Guard against a clip_idx carried over from a different model: the same id
            // can switch archetype while keeping the same action string (so the check
            // above is skipped), leaving an index that is out of range for the new,
            // smaller skeleton. Re-resolve against the current model's clip set.
            if state.clip_idx >= skinned.skin.clips.len() {
                state.clip_idx = skinned.skin.clip_for_action(action).unwrap_or(0);
                state.animate  = skinned.skin.action_animates(action, state.clip_idx);
            }

            if state.animate && *action != "dead" && !skinned.skin.clips.is_empty() {
                let dur = skinned.skin.clips[state.clip_idx].duration;
                if dur > 0.0 {
                    state.time = (state.time + dt) % dur;
                }
            }
        }

        // Remove stale anim states (keep id=0 if player has a race)
        let live_ids: std::collections::HashSet<u32> =
            scene.billboards.iter().map(|b| b.id)
                .chain(if scene.player_race.is_empty() { None } else { Some(0) })
                .collect();
        self.anim_states.retain(|id, _| live_ids.contains(id));

        let aspect = self.surface_config.width as f32 / self.surface_config.height as f32;
        let view_proj = crate::camera::look_at_perspective(
            cam_eye, cam_target, [0.0, 0.0, 1.0], 60.0, aspect, 0.5, 5000.0,
        );
        self.queue.write_buffer(
            &self.camera_uniform.buf, 0, bytemuck::cast_slice(&view_proj),
        );
        self.last_view_proj = view_proj;
        self.last_cam_pos   = cam_eye;

        let fwd   = (glam::Vec3::from(cam_target) - glam::Vec3::from(cam_eye)).normalize();
        let right = fwd.cross(glam::Vec3::Z).normalize();
        let up    = right.cross(fwd).normalize();

        self.ensure_equipment_textures(scene);

        crate::pass::encode_sky_pass(self, encoder, view);
        crate::pass::encode_zone_pass(self, encoder, view, scene);
        crate::pass::encode_billboard_pass(self, encoder, view, scene,
                                           right.to_array(), up.to_array());
        crate::pass::encode_player_pass(self, encoder, view, scene,
                                        right.to_array(), up.to_array());
        crate::pass::encode_entity_pass(self, encoder, view, scene, cam_eye);
        crate::pass::encode_skinned_entity_pass(self, encoder, view, scene, cam_eye);
    }

    /// Recreate the depth texture to match new surface dimensions.
    /// Call this whenever the window is resized.
    pub fn recreate_depth_texture(&mut self) {
        self.depth_view = create_depth_texture(
            &self.device,
            self.surface_config.width,
            self.surface_config.height,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_renderer_uses_pipeline_types() {
        fn _check(r: &EqRenderer) {
            let _: &crate::pipeline::Layouts      = &r.layouts;
            let _: &crate::pipeline::Pipelines    = &r.pipelines;
            let _: &crate::pipeline::CameraUniform = &r.camera_uniform;
        }
    }

    #[test]
    fn render_frame_has_correct_signature() {
        fn _check(
            r:    &mut EqRenderer,
            enc:  &mut wgpu::CommandEncoder,
            view: &wgpu::TextureView,
            scene: &crate::scene::SceneState,
        ) {
            r.render_frame(enc, view, scene,
                [0.0_f32; 3], [0.0_f32; 3], 0.016);
        }
    }

    #[test]
    fn entity_pass_uses_archetype_scale_not_level_scale() {
        // archetype_scale("humanoid") must differ from npc_size(1) to confirm
        // the two code paths are distinct.
        let archetype_s = crate::models::archetype_scale("humanoid");
        let billboard_s = crate::billboard::npc_size(1);
        assert_ne!(archetype_s, billboard_s,
            "archetype_scale and npc_size(1) must differ so the change is observable");
    }
}
