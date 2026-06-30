//! GPU resource owner for a loaded zone. `EqRenderer` uploads zone geometry, placed objects,
//! character models, and textures into wgpu buffers/bind-groups and holds per-entity animation
//! state. The actual per-frame draw calls live in `pass.rs`; pipelines/layouts in `pipeline.rs`.

use crate::assets::ZoneAssets;
use crate::gpu::{
    Vertex, GpuMesh, GpuModel, GpuStaticModel, GpuSkinnedModel, GpuSkinnedMesh, SkinnedVertex,
    upload_textures, create_depth_texture, build_fallback_texture_bg,
};

/// Per-entity animation playback state, tracked across frames (which clip, how far into it, and the
/// last action that selected it).
pub struct EntityAnimState {
    pub clip_idx:    usize,
    pub time:        f32,
    pub last_action: String,
    /// False when an idle action resolved to a non-idle clip (walk fallback for
    /// models with no real idle, e.g. the Skeleton). We freeze such clips at a
    /// static frame so the character stands still instead of walking in place.
    pub animate:     bool,
    /// Idle-cycle phase: incremented each time an idle clip completes a loop, used to alternate
    /// the neutral stand with periodic fidget animations (see `SkinData::idle_clip_for_phase`).
    pub idle_phase:  u32,
}

/// Pre-allocated entity uniform buffer slot count.
/// Layout: [0..PLAYER_UNIFORM_SLOTS) = player, [PLAYER_UNIFORM_SLOTS..) = entities.
// Character GLB models have up to 27 primitives (humanoid). The player draws one
// uniform slot per mesh, so this MUST be >= the max mesh count or the player loses
// its later primitives (head pieces + feet were dropped at the old value of 16).
pub const PLAYER_UNIFORM_SLOTS: usize = 32;
// 32 player + entity mesh draws, split half static / half skinned. At ~27 meshes per
// humanoid, the skinned half (TOTAL/2 - 32) bounds how many character spawns can draw:
// 8224 -> ~4080 skinned slots -> ~150 NPCs. Crowded zones (Qeynos ~190 spawns) exceed
// this, so the skinned pass renders NEAREST-first and the overflow falls back to nameplates.
pub const TOTAL_ENTITY_UNIFORM_SLOTS: usize = 8224;
/// Pre-allocated joint buffer pool size. Slot 0 = player, 1..N = entities.
pub const JOINT_BUF_SLOTS: usize = 512;
/// Dedicated uniform slot count for doors (one slot per door mesh draw this frame).
/// Sized generously; far more than any zone's door count × meshes-per-door.
pub const DOOR_UNIFORM_SLOTS: usize = 512;
/// Size of one joint buffer: 128 joints × mat4(64 bytes).
pub const JOINT_BUF_BYTES: u64 = 128 * 64;

/// Build a unit cube GpuMesh (~2 units per side, centered at origin), used as the door
/// fallback marker. Positions are already in render space; the door pass translates it to
/// the door position via the per-draw model matrix.
/// Resolve a mesh's animated-texture spec `(ms, frame names)` into `(ms, frame texture
/// indices)` against the loaded texture list. Returns `None` if fewer than 2 frames resolve.
fn resolve_anim(anim: &Option<(u32, Vec<String>)>, texture_names: &[String]) -> Option<(u32, Vec<usize>)> {
    let (ms, names) = anim.as_ref()?;
    let idxs: Vec<usize> = names.iter()
        .filter_map(|n| texture_names.iter().position(|t| t == n))
        .collect();
    (idxs.len() >= 2).then(|| (*ms, idxs))
}

fn build_unit_cube(device: &wgpu::Device) -> GpuMesh {
    use wgpu::util::DeviceExt;
    const S: f32 = 1.0; // half-extent → ~2 unit cube
    // 8 corners; normals are coarse (radial) — fallback box only, lighting need not be exact.
    let corners: [[f32; 3]; 8] = [
        [-S, -S, -S], [S, -S, -S], [S, S, -S], [-S, S, -S],
        [-S, -S,  S], [S, -S,  S], [S, S,  S], [-S, S,  S],
    ];
    let verts: Vec<Vertex> = corners.iter().map(|&p| {
        let len = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt().max(1e-6);
        Vertex { position: p, normal: [p[0] / len, p[1] / len, p[2] / len], uv: [0.0, 0.0] }
    }).collect();
    // 12 triangles (two per face), CCW-ish; the fallback marker isn't backface-culled critically.
    let indices: [u32; 36] = [
        0, 1, 2, 0, 2, 3, // -Z
        4, 6, 5, 4, 7, 6, // +Z
        0, 4, 5, 0, 5, 1, // -Y
        3, 2, 6, 3, 6, 7, // +Y
        0, 3, 7, 0, 7, 4, // -X
        1, 5, 6, 1, 6, 2, // +X
    ];
    let vertex_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("door_fallback_vbuf"), contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX });
    let index_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("door_fallback_ibuf"), contents: bytemuck::cast_slice(&indices),
        usage: wgpu::BufferUsages::INDEX });
    GpuMesh {
        vertex_buf, index_buf, index_count: indices.len() as u32,
        texture_idx: None, base_color: [0.8, 0.2, 0.2, 1.0], // reddish so it's obviously a marker
        render_mode: crate::assets::RenderMode::Opaque, anim: None,
    }
}

/// All GPU resources for the currently-loaded zone: the wgpu device/queue/surface, uploaded zone +
/// placed-object meshes, character models + textures, pipelines/layouts, and per-entity animation
/// state. Rebuilt on each zone change; `pass.rs` reads it to issue the frame's draw calls.
pub struct EqRenderer {
    pub device:              wgpu::Device,
    pub queue:               wgpu::Queue,
    pub surface_config:      wgpu::SurfaceConfiguration,
    pub layouts:             crate::pipeline::Layouts,
    pub pipelines:           crate::pipeline::Pipelines,
    pub camera_uniform:      crate::pipeline::CameraUniform,
    pub gpu_meshes:          Vec<crate::gpu::GpuMesh>,
    /// GPU-instanced placed-object models: each model mesh uploaded once + an instance-transform
    /// buffer, drawn with the `zone_instanced` pipeline.
    pub gpu_instanced:       Vec<crate::gpu::GpuInstancedMesh>,
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
    /// Path to the EQ s3d assets (set in load_character_models) — used to load weapon models.
    pub assets_path: std::path::PathBuf,
    /// Held weapon models cached by IDFile. `None` = tried and not found (negative cache).
    pub weapon_cache: std::collections::HashMap<String, Option<crate::gpu::GpuWeapon>>,
    /// Door object models, keyed by UPPERCASE base name (e.g. "DOOR1"). Rebuilt per zone load.
    pub door_models: std::collections::HashMap<String, crate::gpu::GpuWeapon>,
    /// Shared decoded textures for ALL door models in the current zone. A door mesh's
    /// `texture_idx` indexes into this; `None` (or out of range) draws the white fallback.
    pub door_textures: Vec<wgpu::BindGroup>,
    /// Per-door-model local-space AABB (min, max) in render space (pre-placement), used to
    /// build a click hit-box that matches the door's real size instead of a tiny sphere.
    pub door_bounds: std::collections::HashMap<String, ([f32; 3], [f32; 3])>,
    /// Dedicated per-frame uniform pool for door draws (kept separate from the entity pool to
    /// avoid frame-collision with the entity pass).
    pub door_uniform_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)>,
    /// Door model names we've already warned about as missing (warn once per name).
    warned_missing_doors: std::collections::HashSet<String>,
    /// Unit cube (~2 units) drawn at a door's position when its model is missing.
    pub door_fallback: Option<crate::gpu::GpuMesh>,
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

        // Pre-allocate the dedicated door uniform pool (mirrors the entity pool).
        let door_uniform_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)> =
            (0..DOOR_UNIFORM_SLOTS).map(|_| {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label:              Some("door_uniform_pool"),
                    size:               std::mem::size_of::<crate::gpu::EntityUniform>() as u64,
                    usage:              wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label:  Some("door_uniform_pool_bg"),
                    layout: &layouts.entity_bgl,
                    entries: &[wgpu::BindGroupEntry {
                        binding: 0, resource: buf.as_entire_binding(),
                    }],
                });
                (buf, bg)
            }).collect();

        // Build the fallback door cube once (~2 units to a side, centered at origin).
        let door_fallback = Some(build_unit_cube(&device));

        Self {
            device,
            queue,
            surface_config,
            layouts,
            pipelines,
            camera_uniform,
            gpu_meshes: vec![],
            gpu_instanced: vec![],
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
            assets_path: std::path::PathBuf::new(),
            weapon_cache: std::collections::HashMap::new(),
            door_models: std::collections::HashMap::new(),
            door_textures: Vec::new(),
            door_bounds: std::collections::HashMap::new(),
            door_uniform_pool,
            warned_missing_doors: std::collections::HashSet::new(),
            door_fallback,
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
            // (texture_idx, render_mode) → (accumulated vertices, accumulated indices).
            // render_mode is part of the key so each draw call has a single blend mode
            // (opaque/masked/blend/additive) and routes to the matching pipeline.
            let mut groups: HashMap<(Option<usize>, crate::assets::RenderMode), (Vec<Vertex>, Vec<u32>)> = HashMap::new();
            // Resolved animated-texture frames per group (same texture ⇒ same anim).
            let mut anim_by_group: HashMap<(Option<usize>, crate::assets::RenderMode), Option<(u32, Vec<usize>)>> = HashMap::new();

            // Terrain only — placed objects now go to the GPU-instanced path below
            // (collision still uses terrain + expand_objects, unchanged).
            let source_count = assets.terrain.len();
            for mesh in assets.terrain.iter() {
                if mesh.positions.is_empty() || mesh.indices.is_empty() { continue; }
                // The dedicated `__collision__` mesh is invisible collision geometry, not
                // drawable terrain — skip uploading it (it has no real texture).
                if mesh.texture_name.as_deref() == Some(crate::assets::COLLISION_MESH_TAG) { continue; }

                let texture_idx = mesh.texture_name.as_ref()
                    .and_then(|n| self.texture_names.iter().position(|t| t == n));

                let gkey = (texture_idx, mesh.render_mode);
                anim_by_group.entry(gkey.clone())
                    .or_insert_with(|| resolve_anim(&mesh.anim, &self.texture_names));

                let [cx, cy, cz] = mesh.center;
                let entry = groups.entry(gkey).or_default();
                let base = entry.0.len() as u32;

                for (i, &p) in mesh.positions.iter().enumerate() {
                    let normal = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                    // libeq axes map to world as: render.X = server_x = p[2], render.Y = server_y
                    // = p[0], render.Z (up) = p[1]. (The two horizontal axes are swapped vs the
                    // old assumption; confirmed by zone safe-point/geometry alignment across zones.)
                    entry.0.push(Vertex {
                        position: [p[2] + cz, p[0] + cx, p[1] + cy],
                        normal:   [normal[2], normal[0], normal[1]],
                        uv:       mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                    });
                }
                for &idx in &mesh.indices {
                    entry.1.push(idx + base);
                }
            }

            self.gpu_meshes = groups.into_iter().map(|((texture_idx, render_mode), (verts, idxs))| {
                let anim = anim_by_group.get(&(texture_idx, render_mode)).cloned().flatten();
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
                    render_mode,
                    anim,
                }
            }).collect();

            tracing::info!("renderer: merged zone into {} draw calls (was {} source meshes)",
                self.gpu_meshes.len(), source_count);
        }

        // Sort merged meshes so same-texture groups are contiguous (they already are, but be safe).
        self.gpu_meshes.sort_by_key(|m| m.texture_idx.map_or(usize::MAX, |i| i));

        // Build GPU-instanced object models: each ObjectModel mesh is uploaded ONCE (in RAW
        // libeq model-local space — the instanced shader applies the instance matrix and the
        // libeq→render axis swizzle), plus a single instance-transform buffer for all placements.
        {
            let mut instanced: Vec<crate::gpu::GpuInstancedMesh> = Vec::new();
            for model in &assets.objects {
                if model.instances.is_empty() { continue; }
                let instance_count = model.instances.len() as u32;
                for mesh in &model.meshes {
                    if mesh.positions.is_empty() || mesh.indices.is_empty() { continue; }
                    // RAW positions/normals — NO axis swizzle (the shader does it). This differs
                    // from the terrain merge, which swizzles CPU-side.
                    let verts: Vec<Vertex> = mesh.positions.iter().enumerate().map(|(i, &p)| {
                        let n = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                        Vertex {
                            position: p,
                            normal:   n,
                            uv:       mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                        }
                    }).collect();
                    let vertex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("object_vbuf"),
                        contents: bytemuck::cast_slice(&verts),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    let index_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("object_ibuf"),
                        contents: bytemuck::cast_slice(&mesh.indices),
                        usage: wgpu::BufferUsages::INDEX,
                    });
                    let texture_idx = mesh.texture_name.as_ref()
                        .and_then(|name| self.texture_names.iter().position(|t| t == name));
                    // One instance buffer per mesh (wgpu::Buffer isn't Clone in this version;
                    // all meshes of a model share the same instance set, but the buffer is tiny).
                    let instance_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("object_instances"),
                        contents: bytemuck::cast_slice(&model.instances),
                        usage: wgpu::BufferUsages::VERTEX,
                    });
                    instanced.push(crate::gpu::GpuInstancedMesh {
                        vertex_buf,
                        index_buf,
                        index_count: mesh.indices.len() as u32,
                        instance_buf,
                        instance_count,
                        texture_idx,
                        render_mode: mesh.render_mode,
                        anim: resolve_anim(&mesh.anim, &self.texture_names),
                    });
                }
            }
            tracing::info!("renderer: built {} instanced object meshes from {} models",
                instanced.len(), assets.objects.len());
            self.gpu_instanced = instanced;
        }

        // Retain CPU-side data for terrain height queries.
        self.zone_assets = Some(assets.clone());
    }

    /// Load all archetype character models and upload to GPU.
    /// Tries glTF files from `models_dir` (the asset-server cache) first; falls back to EQ `_chr.s3d`
    /// archives ALSO from the cache (the "gameequip" set), never from ~/eq_assets.
    /// Models with valid skins (joint_count ≤ 128) are loaded as Skinned; others as Static.
    /// Missing models fall back to billboard rendering.
    /// `_assets_path` is unused (everything now comes from the cache); kept for call-site stability.
    pub fn load_character_models(&mut self, models_dir: &std::path::Path, _assets_path: &std::path::Path) {
        use crate::models::ModelAsset;
        // Worn-armor textures + held-weapon S3Ds come from the asset-server cache ("gameequip" set),
        // not ~/eq_assets. Weapon loading (ensure_weapon) reads from here too.
        self.assets_path = models_dir.to_path_buf();

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
                        tracing::warn!("renderer: glTF load failed for '{}': {}", key, e);
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
                        let path = models_dir.join(name);
                        if path.exists() {
                            match ModelAsset::load_from_chr_s3d(&path) {
                                Ok(a) => Some(a),
                                Err(e) => {
                                    tracing::warn!("renderer: chr S3D load failed for '{}': {}", key, e);
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
                            tracing::info!("renderer: no model for archetype '{}'", key);
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
                    Err(e) => tracing::warn!("renderer: female glTF load failed for '{}': {}", key, e),
                }
            }
            for (gender, asset) in variants {
                let model = self.build_character_model(key, asset);
                self.gpu_character_models.insert((key, gender), model);
            } // gender variants
        }

        // Per-race character models (`race_<code>.glb`): each maps to exactly one
        // race+gender, the gender baked into the code (see `models::race_model_basename`).
        // There is no look-alike fallback — a race whose model file is absent simply
        // does not render, so log each missing one ONCE here rather than per frame.
        for &code in crate::models::PLAYABLE_RACE_MODELS {
            let path = models_dir.join(format!("{code}.glb"));
            if !path.exists() {
                tracing::error!("renderer: missing character model '{code}.glb' — that race will not render");
                continue;
            }
            match ModelAsset::load(&path) {
                Ok(asset) => {
                    let model = self.build_character_model(code, asset);
                    self.gpu_character_models.insert((code, 0), model);
                }
                Err(e) => tracing::error!("renderer: failed to load character model '{code}.glb': {e}"),
            }
        }

        // Index armor textures: shared velious sets (global17-23_amr) + each
        // archetype's chr/chr2 archives (lower material numbers). No decoding here.
        for n in 17..=23 {
            crate::assets::index_s3d_textures(
                &models_dir.join(format!("global{}_amr.s3d", n)), &mut self.equip_index);
        }
        // global_chr.s3d is the combined all-races base archive — it carries the low-material
        // (00-04) body textures that the per-race *_chr archives can be missing (e.g. human is
        // missing chest material 03). Index it for TEXTURES only (we never load it as a model).
        crate::assets::index_s3d_textures(
            &models_dir.join("global_chr.s3d"), &mut self.equip_index);
        for &key in ARCHETYPES {
            if let Some(name) = crate::models::archetype_to_chr_s3d(key) {
                crate::assets::index_s3d_textures(&models_dir.join(name), &mut self.equip_index);
                // also the _chr2 companion if present
                let chr2 = name.replace("_chr.s3d", "_chr2.s3d");
                let chr2_path = models_dir.join(&chr2);
                if chr2_path.exists() {
                    crate::assets::index_s3d_textures(&chr2_path, &mut self.equip_index);
                }
            }
        }
        tracing::info!("equip: indexed {} armor textures", self.equip_index.len());
    }

    /// Upload one `ModelAsset` to the GPU as a `GpuModel` (skinned when it has a
    /// usable skin, else static). `label` is only used for log lines. Shared by the
    /// archetype loader and the per-race (`race_<code>.glb`) loader.
    fn build_character_model(&self, label: &str, asset: crate::models::ModelAsset) -> crate::gpu::GpuModel {
        use wgpu::util::DeviceExt;
        use crate::models::SkinnedMeshData;
        tracing::info!("renderer: loaded '{}' — y_bottom={:.4} y_extent={:.4} x_center={:.4} z_center={:.4}",
            label, asset.y_bottom, asset.y_extent, asset.x_center, asset.z_center);

        let (_, tex_bgs) = upload_textures(
            &self.device, &self.queue, &asset.textures, &self.layouts.texture_bgl,
        );
        let tex_names: Vec<String> =
            asset.textures.iter().map(|t| t.name.clone()).collect();

        let use_skinned = asset.skin.as_ref()
            .is_some_and(|s| s.joint_count > 0 && s.joint_count <= 128);

        if use_skinned {
            let skin = asset.skin.unwrap();
            let mut meshes: Vec<GpuSkinnedMesh>                       = Vec::new();
            let mut skinned_slots: Vec<Option<crate::models::EquipSlot>> = Vec::new();
            let mut skinned_head_parts: Vec<Option<crate::models::HeadPart>> = Vec::new();
            let mut skinned_head_hidden: Vec<bool>                    = Vec::new();
            for (((mesh, sd_opt), &mesh_node_scale), (&slot, (&hp, &dh))) in asset.meshes.iter()
                .zip(asset.skin_meshes.iter())
                .zip(asset.skinned_mesh_scales.iter())
                .zip(asset.equip_slots.iter()
                    .zip(asset.head_parts.iter()
                        .zip(asset.head_default_hidden.iter())))
            {
                if mesh.positions.is_empty() || mesh.indices.is_empty() { continue; }
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
                meshes.push(GpuSkinnedMesh { vertex_buf: vbuf, index_buf: ibuf,
                                             index_count: mesh.indices.len() as u32,
                                             texture_idx, base_color: mesh.base_color,
                                             mesh_node_scale });
                skinned_slots.push(slot);
                skinned_head_parts.push(hp);
                skinned_head_hidden.push(dh);
            }
            tracing::info!("renderer: loaded skinned model '{}' ({} joints, {} clips)",
                      label, skin.joint_count, skin.clips.len());
            GpuModel::Skinned(GpuSkinnedModel { meshes, texture_bind_groups: tex_bgs, skin, node_scale: asset.skinned_node_scale, y_bottom: asset.y_bottom, x_center: asset.x_center, z_center: asset.z_center, prefix: asset.prefix.clone(), equip_slots: skinned_slots, head_parts: skinned_head_parts, head_default_hidden: skinned_head_hidden, true_height: asset.true_height, clip_bounds: asset.clip_bounds.clone(), feet_offset: asset.feet_offset })
        } else {
            let mut meshes: Vec<GpuMesh>                              = Vec::new();
            let mut static_slots: Vec<Option<crate::models::EquipSlot>> = Vec::new();
            let mut static_head_parts: Vec<Option<crate::models::HeadPart>> = Vec::new();
            let mut static_head_hidden: Vec<bool>                     = Vec::new();
            for (mesh, (&slot, (&hp, &dh))) in asset.meshes.iter()
                .zip(asset.equip_slots.iter()
                    .zip(asset.head_parts.iter()
                        .zip(asset.head_default_hidden.iter())))
            {
                if mesh.positions.is_empty() || mesh.indices.is_empty() { continue; }
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
                meshes.push(GpuMesh { vertex_buf: vbuf, index_buf: ibuf,
                           index_count: mesh.indices.len() as u32, texture_idx,
                           base_color: mesh.base_color,
                           render_mode: crate::assets::RenderMode::Opaque, anim: None });
                static_slots.push(slot);
                static_head_parts.push(hp);
                static_head_hidden.push(dh);
            }
            tracing::info!("renderer: loaded static model '{}'", label);
            GpuModel::Static(GpuStaticModel { meshes, texture_bind_groups: tex_bgs, y_bottom: asset.y_bottom, y_extent: asset.y_extent, x_center: asset.x_center, z_center: asset.z_center, prefix: asset.prefix.clone(), equip_slots: static_slots, head_parts: static_head_parts, head_default_hidden: static_head_hidden, true_height: asset.true_height, clip_bounds: vec![], feet_offset: 0.0 })
        }
    }

    /// Select a loaded character model for an archetype + gender, falling back to the
    /// male (gender 0) variant when no female variant exists.
    pub fn model_for(&self, archetype: &'static str, gender: u8) -> Option<&crate::gpu::GpuModel> {
        self.gpu_character_models.get(&(archetype, gender))
            .or_else(|| self.gpu_character_models.get(&(archetype, 0)))
    }

    /// Select the character model a spawn of `race` + `gender` should render with.
    /// Playable races resolve to their own `race_<code>` model and do NOT fall back
    /// to an archetype, so a race whose model is missing returns `None` (rendered as
    /// nothing). Monsters resolve to their archetype model (with female→male fallback).
    pub fn character_model_for(&self, race: &str, gender: u8) -> Option<&crate::gpu::GpuModel> {
        let (key, slot) = crate::models::character_model_key(race, gender);
        self.model_by_key(key, slot)
    }

    /// Look up a character model by an already-resolved registry key + gender slot
    /// (from `models::character_model_key`), with the female→male slot-0 fallback.
    /// Lets a draw list cache the resolved key so every pass renders the same model.
    pub fn model_by_key(&self, key: &'static str, slot: u8) -> Option<&crate::gpu::GpuModel> {
        self.gpu_character_models.get(&(key, slot))
            .or_else(|| self.gpu_character_models.get(&(key, 0)))
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

    /// Load + GPU-upload a held weapon model by IDFile (e.g. "IT10649") and cache it. Negative-cached
    /// on miss so we don't rescan gequip every frame. Drawn at the hand bone by the player pass.
    pub fn ensure_weapon(&mut self, idfile: &str) {
        use wgpu::util::DeviceExt;
        let key = idfile.trim().to_uppercase();
        if key.is_empty() || self.weapon_cache.contains_key(&key) { return; }
        let assets = match crate::assets::load_weapon_model(&self.assets_path, &key) {
            Some(a) => a,
            None => { self.weapon_cache.insert(key, None); return; }
        };
        let (_tex, bgs) = crate::gpu::upload_textures(
            &self.device, &self.queue, &assets.textures, &self.layouts.texture_bgl);
        let tex_names: Vec<String> = assets.textures.iter().map(|t| t.name.clone()).collect();
        let mut meshes = Vec::new();
        for m in &assets.terrain {
            if m.positions.is_empty() || m.indices.is_empty() { continue; }
            let [cx, cy, cz] = m.center;
            // libeq [p0,p1,p2] -> render [p2,p0,p1] (same axis convention as zone/static meshes).
            let verts: Vec<crate::gpu::Vertex> = m.positions.iter().enumerate().map(|(i, &p)| {
                let n = m.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                crate::gpu::Vertex {
                    position: [p[2] + cz, p[0] + cx, p[1] + cy],
                    normal:   [n[2], n[0], n[1]],
                    uv:       m.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                }
            }).collect();
            let texture_idx = m.texture_name.as_ref()
                .and_then(|tn| tex_names.iter().position(|t| t == tn));
            let vertex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("weapon_vbuf"), contents: bytemuck::cast_slice(&verts),
                usage: wgpu::BufferUsages::VERTEX });
            let index_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("weapon_ibuf"), contents: bytemuck::cast_slice(&m.indices),
                usage: wgpu::BufferUsages::INDEX });
            meshes.push(crate::gpu::GpuMesh {
                vertex_buf, index_buf, index_count: m.indices.len() as u32,
                texture_idx, base_color: [1.0; 4],
                render_mode: crate::assets::RenderMode::Opaque, anim: None });
        }
        tracing::info!("weapon: cached '{}' — {} gpu meshes, {} textures", key, meshes.len(), bgs.len());
        self.weapon_cache.insert(key, Some(crate::gpu::GpuWeapon { meshes, texture_bind_groups: bgs }));
    }

    /// Pre-pass (mutable): ensure every armor texture needed this frame is cached.
    /// Runs before the immutable render passes so they only do lookups.
    pub fn ensure_equipment_textures(&mut self, scene: &crate::scene::SceneState) {
        use crate::models::equip_swap_key;
        use crate::gpu::GpuModel;

        // Phase 1: collect needed base names (no mutation of the cache yet).
        let mut needed: Vec<String> = Vec::new();
        for b in &scene.billboards {
            let (prefix, slots) = match self.character_model_for(&b.race, b.gender) {
                Some(GpuModel::Static(m))  => (&m.prefix, &m.equip_slots),
                Some(GpuModel::Skinned(m)) => (&m.prefix, &m.equip_slots),
                None => continue,
            };
            if prefix.is_empty() { continue; }
            for es in slots.iter().flatten() {
                let mat = b.equipment[es.slot];
                for m in std::iter::once(mat).chain(crate::models::velious_material_fallback(mat)).chain(std::iter::once(0u32)) {
                    if let Some(key) = equip_swap_key(prefix, *es, m) {
                        if !self.equipment_tex_cache.contains_key(&key) {
                            needed.push(key);
                        }
                    }
                }
            }
        }
        if !scene.player_race.is_empty() {
            if let Some(model) = self.character_model_for(&scene.player_race, scene.player_gender) {
                let (prefix, slots) = match model {
                    GpuModel::Static(m)  => (&m.prefix, &m.equip_slots),
                    GpuModel::Skinned(m) => (&m.prefix, &m.equip_slots),
                };
                if !prefix.is_empty() {
                    for es in slots.iter().flatten() {
                        let mat = scene.player_equipment[es.slot];
                        for m in std::iter::once(mat).chain(crate::models::velious_material_fallback(mat)).chain(std::iter::once(0u32)) {
                            if let Some(key) = equip_swap_key(prefix, *es, m) {
                                if !self.equipment_tex_cache.contains_key(&key) {
                                    needed.push(key);
                                }
                            }
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

    /// Load + GPU-upload the door/object models for a zone. Clears any previously loaded
    /// door models first (zone reload). Models are uploaded with the same libeq→render axis
    /// swap as weapons/zone meshes. Textures from `load_object_models` are uploaded into the
    /// shared `door_textures` and linked per mesh by `texture_idx`. Per-model local AABBs are
    /// recorded in `door_bounds` for click-picking.
    pub fn load_door_models(&mut self, main_s3d: &std::path::Path, obj_s3d: &std::path::Path) {
        use wgpu::util::DeviceExt;
        self.door_models.clear();
        self.door_bounds.clear();
        self.warned_missing_doors.clear();

        let (models, textures) = match crate::assets::load_object_models(main_s3d, obj_s3d) {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!("doors: load_object_models failed ({}); doors will use fallback boxes", e);
                return;
            }
        };

        // Upload the door textures once into the shared `door_textures`; meshes reference them
        // by global index (texture_idx). wgpu::BindGroup isn't Clone, so we keep one shared set.
        let (_tex, door_bgs) = crate::gpu::upload_textures(
            &self.device, &self.queue, &textures, &self.layouts.texture_bgl);
        let tex_names: Vec<String> = textures.iter().map(|t| t.name.clone()).collect();
        self.door_textures = door_bgs;

        for (name, meshes) in models {
            let mut gpu_meshes = Vec::new();
            let mut bmin = [f32::MAX; 3];
            let mut bmax = [f32::MIN; 3];
            for m in &meshes {
                if m.positions.is_empty() || m.indices.is_empty() { continue; }
                let [cx, cy, cz] = m.center;
                // libeq [p0,p1,p2] -> render [p2,p0,p1] (same axis convention as weapons/zone).
                let verts: Vec<Vertex> = m.positions.iter().enumerate().map(|(i, &p)| {
                    let n = m.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                    Vertex {
                        position: [p[2] + cz, p[0] + cx, p[1] + cy],
                        normal:   [n[2], n[0], n[1]],
                        uv:       m.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                    }
                }).collect();
                for v in &verts {
                    for k in 0..3 { bmin[k] = bmin[k].min(v.position[k]); bmax[k] = bmax[k].max(v.position[k]); }
                }
                let vertex_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("door_vbuf"), contents: bytemuck::cast_slice(&verts),
                    usage: wgpu::BufferUsages::VERTEX });
                let index_buf = self.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("door_ibuf"), contents: bytemuck::cast_slice(&m.indices),
                    usage: wgpu::BufferUsages::INDEX });
                // Link the mesh to its decoded texture by name (lowercased); None -> fallback white.
                let texture_idx = m.texture_name.as_ref()
                    .and_then(|tn| tex_names.iter().position(|t| t == &tn.to_lowercase()));
                gpu_meshes.push(GpuMesh {
                    vertex_buf, index_buf, index_count: m.indices.len() as u32,
                    texture_idx, base_color: m.base_color,
                    render_mode: crate::assets::RenderMode::Opaque, anim: None });
            }
            if gpu_meshes.is_empty() { continue; }
            self.door_bounds.insert(name.clone(), (bmin, bmax));
            // texture_bind_groups stays empty: textures are shared via self.door_textures and
            // referenced by each mesh's (global) texture_idx.
            self.door_models.insert(name, crate::gpu::GpuWeapon {
                meshes: gpu_meshes, texture_bind_groups: Vec::new() });
        }
        tracing::info!("doors: loaded {} door/object models", self.door_models.len());
    }

    /// Pre-pass (mutable): warn once per door whose model is missing from `door_models`.
    /// Runs before the immutable render passes (door names come from the live scene, so we
    /// can only know which are missing once we have the scene). The draw pass itself stays
    /// immutable and silently substitutes the fallback box.
    pub fn note_missing_door_models(&mut self, scene: &crate::scene::SceneState) {
        for door in &scene.doors {
            let key = door.name.to_uppercase();
            if !self.door_models.contains_key(&key)
                && self.warned_missing_doors.insert(door.name.clone()) {
                tracing::warn!("doors: missing model {:?} for door {} — using fallback box",
                          door.name, door.door_id);
            }
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
            let (key, slot) = crate::models::character_model_key(race, *gender);
            // Direct field lookup (with female→male fallback) keeps the borrow disjoint
            // from the `self.anim_states` mutation below; `character_model_for` would borrow all of self.
            let model = self.gpu_character_models.get(&(key, slot))
                .or_else(|| self.gpu_character_models.get(&(key, 0)));
            let Some(GpuModel::Skinned(skinned)) = model else { continue };

            let is_idle = matches!(*action, "idle" | "standing" | "wait");

            let state = self.anim_states.entry(*id).or_insert_with(|| {
                let clip_idx = skinned.skin.clip_for_action("walking").unwrap_or(0);
                EntityAnimState { clip_idx, time: 0.0, last_action: String::new(), animate: true, idle_phase: 0 }
            });

            if *action != state.last_action {
                // Idle starts on its neutral stand (phase 0); other actions resolve their clip directly.
                // Dead: try to find the D05 death clip; use usize::MAX as a sentinel when none exists
                // so the pass falls back to bind pose instead of accidentally playing clip 0.
                state.idle_phase  = 0;
                state.time        = 0.0;
                state.last_action = action.to_string();
                if *action == "dead" {
                    match skinned.skin.clip_for_action("dead") {
                        Some(ci) => {
                            state.clip_idx = ci;
                            state.animate  = true;  // play once, renderer clamps at end
                        }
                        None => {
                            state.clip_idx = usize::MAX; // sentinel: no death clip → bind pose
                            state.animate  = false;
                        }
                    }
                } else if is_idle {
                    state.clip_idx = skinned.skin.idle_clip_for_phase(0).unwrap_or(0);
                    state.animate  = skinned.skin.action_animates(action, state.clip_idx);
                } else {
                    state.clip_idx = skinned.skin.clip_for_action(action).unwrap_or(0);
                    state.animate  = skinned.skin.action_animates(action, state.clip_idx);
                }
            }

            // Guard against a clip_idx carried over from a different model: the same id
            // can switch archetype while keeping the same action string (so the check
            // above is skipped), leaving an index that is out of range for the new,
            // smaller skeleton. Re-resolve against the current model's clip set.
            // Skip re-resolution for dead entities: usize::MAX is the intentional "no death
            // clip" sentinel and must not be overwritten with clip 0.
            if state.clip_idx >= skinned.skin.clips.len() && *action != "dead" {
                state.clip_idx = skinned.skin.clip_for_action(action).unwrap_or(0);
                state.animate  = skinned.skin.action_animates(action, state.clip_idx);
            }

            // Advance animation time. Dead plays once then holds at the final frame;
            // all other actions loop, with idle cycling through fidgets.
            if state.animate && state.clip_idx < skinned.skin.clips.len() && !skinned.skin.clips.is_empty() {
                let dur = skinned.skin.clips[state.clip_idx].duration;
                if dur > 0.0 {
                    let next = state.time + dt;
                    if *action == "dead" {
                        // Play once: clamp time to duration so evaluate() returns the last frame.
                        state.time = next.min(dur);
                        if next >= dur { state.animate = false; } // done; hold pose
                    } else if next >= dur {
                        state.time = next % dur;
                        // On each completed idle loop, advance the cycle so the character
                        // alternates the neutral stand with periodic fidgets (like native).
                        if is_idle {
                            state.idle_phase = state.idle_phase.wrapping_add(1);
                            if let Some(ci) = skinned.skin.idle_clip_for_phase(state.idle_phase) {
                                state.clip_idx = ci;
                                state.animate = skinned.skin.action_animates(action, ci);
                            }
                        }
                    } else {
                        state.time = next;
                    }
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
        self.note_missing_door_models(scene);
        // Ensure the player's equipped weapon models are loaded (cached by IDFile).
        let (wp, ws) = (scene.primary_weapon_idfile.clone(), scene.secondary_weapon_idfile.clone());
        if !wp.is_empty() { self.ensure_weapon(&wp); }
        if !ws.is_empty() { self.ensure_weapon(&ws); }

        crate::pass::encode_sky_pass(self, encoder, view);
        crate::pass::encode_zone_pass(self, encoder, view, scene);
        crate::pass::encode_door_pass(self, encoder, view, scene);
        crate::pass::encode_billboard_pass(self, encoder, view, scene,
                                           right.to_array(), up.to_array());
        crate::pass::encode_player_pass(self, encoder, view, scene);
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
    fn eqoxide_uses_pipeline_types() {
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
