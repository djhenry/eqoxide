//! The per-frame render pass. Draws the zone terrain + placed objects, skinned characters (player
//! and NPCs, with equipment-texture swaps), camera-facing billboards/nameplates, and the egui HUD.
//! Reads GPU resources from `EqRenderer` and "what to draw" from `SceneState`. The armor-texture
//! selection + `equip_mesh_hidden` logic here is documented in `docs/equipment-textures-findings.md`.

use crate::renderer::EqRenderer;
use crate::scene::SceneState;

/// Milliseconds since the first call — the clock that drives animated zone textures
/// (fire/water/lava). Process-global so every frame shares one monotonic timeline.
fn anim_now_ms() -> u64 {
    use std::sync::OnceLock;
    use std::time::Instant;
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Entity draw distance (EQ units, measured from the player). Beyond this an NPC's 3D
/// model is not drawn — it's a distant speck. Combined with a frustum test, this caps
/// the per-frame entity work in densely-populated zones (e.g. gfaydark, ~400 spawns).
pub(crate) const ENTITY_DRAW_DIST: f32 = 500.0;
/// NDC slack for the frustum test so a tall model whose feet sit just off-screen still
/// draws (the culled position is the feet; the body extends upward). Shared with the HUD so
/// nameplates cull on the exact same distance+frustum test as models (#177).
pub(crate) const ENTITY_CULL_MARGIN: f32 = 0.5;

/// Vestigial: this used to HIDE an armor mesh whose exact material+variant texture was
/// missing (e.g. the variant-03 main chest torso for an armor material that only ships
/// variants 01/02). But the chest variant pieces are DISJOINT (zero shared verts), so
/// hiding the textureless torso left a see-through hole (a "transparent chest") rather than
/// revealing a sibling. `resolve_overlay_tex` now falls back to the material-0 base cloth
/// for such pieces, so nothing ever needs hiding. Kept as a no-op so the call sites in the
/// two-pass body draw stay readable; always returns false.
fn equip_mesh_hidden(
    _r: &EqRenderer, _prefix: &str,
    _slot: Option<crate::models::EquipSlot>, _equipment: &[u32; 9],
) -> bool {
    false
}

fn resolve_equip_tex<'a>(
    r:          &'a EqRenderer,
    baked_bgs:  &'a [wgpu::BindGroup],
    baked_idx:  Option<usize>,
    prefix:     &str,
    slot:       Option<crate::models::EquipSlot>,
    equipment:  &[u32; 9],
) -> &'a wgpu::BindGroup {
    if let Some(es) = slot {
        let mat = equipment[es.slot];
        // equip_swap_key returns None for material 0 (naked → baked texture) / no prefix.
        if let Some(key) = crate::models::equip_swap_key(prefix, es.clone(), mat) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) {
                return bg;
            }
        }
        // Velious-range (17-23) fallback: the raw racial texture (e.g. elflg2301) often doesn't
        // exist, so remap to the classic base tier (e.g. 23 → 1 leather) like the original client.
        if let Some(rmat) = crate::models::velious_material_fallback(mat) {
            if let Some(key) = crate::models::equip_swap_key(prefix, es, rmat) {
                if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) {
                    return bg;
                }
            }
        }
    }
    match baked_idx {
        Some(i) if i < baked_bgs.len() => &baked_bgs[i],
        _ => &r.fallback_texture_bg,
    }
}

/// Skin-base bind group for a body mesh: the model's own baked texture (the Luclin skin layer the
/// WLD material palette references by default), or the white fallback if the mesh has none.
fn skin_base_tex<'a>(
    r: &'a EqRenderer, baked_bgs: &'a [wgpu::BindGroup], baked_idx: Option<usize>,
) -> &'a wgpu::BindGroup {
    match baked_idx {
        Some(i) if i < baked_bgs.len() => &baked_bgs[i],
        _ => &r.fallback_texture_bg,
    }
}

/// The cloth/armor OVERLAY bind group for a body slot, if a usable swapped texture is cached.
/// Unlike `resolve_equip_tex`, this returns `None` (rather than the baked skin) when there is no
/// overlay — material-0 skin regions, rejected transparent stubs, and missing textures. The
/// two-pass renderer draws the skin base first, then this overlay alpha-blended on top, so a
/// `None` here means bare skin shows (e.g. the elf-female exposed midriff).
fn resolve_overlay_tex<'a>(
    r: &'a EqRenderer, prefix: &str,
    slot: Option<crate::models::EquipSlot>, equipment: &[u32; 9],
) -> Option<&'a wgpu::BindGroup> {
    let es = slot?;
    let mat = equipment[es.slot];
    if let Some(key) = crate::models::equip_swap_key(prefix, es.clone(), mat) {
        if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
    }
    if let Some(rmat) = crate::models::velious_material_fallback(mat) {
        if let Some(key) = crate::models::equip_swap_key(prefix, es.clone(), rmat) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
        }
    }
    // Base-cloth fallback: a body region whose armor material lacks a texture for THIS
    // variant stays clothed instead of vanishing. The chest's disjoint variant pieces
    // don't all ship per material (e.g. material 3 has chest variants 01/02 but not the
    // main 03 torso), so without this the textureless piece would be hidden into a
    // see-through hole. Skin regions (he/hn/ft) return None at material 0 → bare skin.
    if mat != 0 {
        if let Some(key) = crate::models::equip_swap_key(prefix, es, 0) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
        }
    }
    // Final base-cloth fallback: a BODY piece whose exact variant's baseline-cloth texture was
    // never extracted still renders clothed by borrowing variant-01 of the same region. The male
    // equip-texture sets are incomplete (e.g. wood-elf male ships elmch0001/0002 but NOT elmch0003,
    // and only variant-01 of the arms) where the female set is complete — so without this, male
    // humanoids rendered bare on exactly those pieces while females looked fine. Skin regions
    // (he/hn/ft) are excluded so bare hands/head/feet stay skin. (eqoxide#82)
    if !matches!(&es.region, b"he" | b"hn" | b"ft") && es.variant != 1 {
        let base = crate::models::EquipSlot { variant: 1, ..es };
        if let Some(key) = crate::models::equip_swap_key(prefix, base, 0) {
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) { return Some(bg); }
        }
    }
    None
}

/// Sky gradient background pass. MUST be called before all other passes.
/// Fills the color buffer with the gradient; subsequent passes draw on top.
/// No depth attachment — sky is purely a background layer.
pub fn encode_sky_pass(
    r:       &EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("sky"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load:  wgpu::LoadOp::Clear(wgpu::Color { r: 0.74, g: 0.86, b: 0.97, a: 1.0 }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.sky);
    pass.draw(0..6, 0..1);
}

/// Zone geometry pass. Clears depth to 1.0; preserves sky color from sky pass.
pub fn encode_zone_pass(
    r:       &EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
    _scene:  &SceneState,
) {
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("zone"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Clear(1.0),  // only pass that clears depth
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
    });

    use crate::assets::RenderMode;
    let tex_bg = |idx: Option<usize>| -> &wgpu::BindGroup {
        match idx {
            Some(i) if i < r.texture_bind_groups.len() => &r.texture_bind_groups[i],
            _ => &r.fallback_texture_bg,
        }
    };
    // Wall-clock ms since process start, for cycling animated textures (fire/water/lava).
    let now_ms = anim_now_ms();
    // The texture a mesh should bind this frame: its current animation frame if animated,
    // else its static texture. The per-loop texture cache naturally rebinds when it advances.
    let frame_tex = |tex: Option<usize>, anim: &Option<(u32, Vec<usize>)>| -> Option<usize> {
        match anim {
            Some((ms, frames)) if !frames.is_empty() => {
                Some(frames[(now_ms / (*ms).max(1) as u64) as usize % frames.len()])
            }
            _ => tex,
        }
    };

    // Draw the static terrain meshes whose render_mode is in `modes`, with `pipeline`.
    // Opaque + masked share the opaque pipeline (masked discards in-shader, depth-write
    // on); blend/additive run after with their own depth-write-off pipelines.
    let draw_static = |pass: &mut wgpu::RenderPass<'_>, pipeline, modes: &[RenderMode]| {
        let mut bound = false;
        let mut current_tex: Option<usize> = None;
        for mesh in &r.gpu_meshes {
            if !modes.contains(&mesh.render_mode) { continue; }
            let etex = frame_tex(mesh.texture_idx, &mesh.anim);
            if !bound {
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                pass.set_bind_group(1, tex_bg(etex), &[]);
                current_tex = etex;
                bound = true;
            } else if etex != current_tex {
                current_tex = etex;
                pass.set_bind_group(1, tex_bg(current_tex), &[]);
            }
            pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    };
    draw_static(&mut pass, &r.pipelines.zone, &[RenderMode::Opaque, RenderMode::Masked]);

    // ── GPU-instanced placed objects ───────────────────────────────────────
    let draw_instanced = |pass: &mut wgpu::RenderPass<'_>, pipeline, modes: &[RenderMode]| {
        let mut bound = false;
        let mut current_tex: Option<usize> = None;
        for mesh in &r.gpu_instanced {
            if !modes.contains(&mesh.render_mode) { continue; }
            let etex = frame_tex(mesh.texture_idx, &mesh.anim);
            if !bound {
                pass.set_pipeline(pipeline);
                pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                pass.set_bind_group(1, tex_bg(etex), &[]);
                current_tex = etex;
                bound = true;
            } else if etex != current_tex {
                current_tex = etex;
                pass.set_bind_group(1, tex_bg(current_tex), &[]);
            }
            pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            pass.set_vertex_buffer(1, mesh.instance_buf.slice(..));
            pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..mesh.instance_count);
        }
    };
    draw_instanced(&mut pass, &r.pipelines.zone_instanced, &[RenderMode::Opaque, RenderMode::Masked]);

    // ── Transparent passes (after all opaque/masked, depth-write off) ───────
    draw_static(&mut pass, &r.pipelines.zone_blend, &[RenderMode::Blend]);
    draw_instanced(&mut pass, &r.pipelines.zone_instanced_blend, &[RenderMode::Blend]);
    draw_static(&mut pass, &r.pipelines.zone_additive, &[RenderMode::Additive]);
    draw_instanced(&mut pass, &r.pipelines.zone_instanced_additive, &[RenderMode::Additive]);
}

/// Draw the zone's doors (closed state). Each door uses its object model if loaded, else a
/// reddish fallback cube at the door position. Per-door model matrix lets Task 9 animate opens.
/// Each mesh binds its decoded texture from the shared `door_textures` (by `texture_idx`),
/// falling back to the white placeholder only when a model/texture is missing.
///
/// Placement (closed): `m = translate(pos) * rotZ(yaw) * rotY(incline) * scale(size/100)`,
/// `yaw = heading*TAU/512 + FRAC_PI_2`. The door model's origin is the hinge edge (= door.pos),
/// so the open animation swings about the origin.
pub fn encode_door_pass(
    r:       &EqRenderer,
    encoder: &mut wgpu::CommandEncoder,
    view:    &wgpu::TextureView,
    scene:   &SceneState,
) {
    use crate::gpu::EntityUniform;
    if scene.doors.is_empty() { return; }

    // Phase 1: assign a uniform slot per door, write its model matrix, and record what to draw.
    // (slot_idx, &GpuMesh) — meshes of the same door share that door's slot/matrix.
    // (slot, mesh, texture bind group) — None texture falls back to the white placeholder.
    let mut draws: Vec<(usize, &crate::gpu::GpuMesh, Option<&wgpu::BindGroup>)> = Vec::new();
    let mut slot = 0usize;
    for door in &scene.doors {
        if slot >= r.door_uniform_pool.len() { break; }

        let model_meshes: Vec<&crate::gpu::GpuMesh> =
            match r.door_models.get(&door.name.to_uppercase()) {
                Some(w) => w.meshes.iter().collect(),
                None    => match &r.door_fallback {
                    Some(cube) => vec![cube],
                    None       => continue,
                },
            };
        if model_meshes.is_empty() { continue; }

        // Build the placement matrix. Fallback cube uses translate-only (no model orientation).
        let key = door.name.to_uppercase();
        let mat = if r.door_models.contains_key(&key) {
            let scale = door.size as f32 / 100.0;
            // Door heading is raw EQ units (0..512). The +90° offset (verified visually: doors
            // face the correct way with it) matches the entity/player convention:
            // yaw = heading*TAU/512 + FRAC_PI_2.
            let yaw   = (door.heading / 512.0) * std::f32::consts::TAU + std::f32::consts::FRAC_PI_2;
            let placement = glam::Mat4::from_translation(glam::Vec3::from(door.pos))
                * glam::Mat4::from_rotation_z(yaw)
                * glam::Mat4::from_rotation_y((door.incline as f32 / 512.0) * std::f32::consts::TAU)
                * glam::Mat4::from_scale(glam::Vec3::splat(scale));

            // Apply open animation in door-local model space (after scale).
            let f = door.open_frac;
            let local_open = match door.opentype {
                100..=119 => glam::Mat4::from_translation(glam::vec3(0.0, 0.0, 10.0 * f)),
                11..=15   => glam::Mat4::from_translation(glam::vec3(8.0 * f, 0.0, 0.0)),
                _ => {
                    // Hinged swing about the model origin (= door.pos = the hinge edge in EQ).
                    // Negative angle swings the leaf outward (away from the player side).
                    glam::Mat4::from_rotation_z(-f * std::f32::consts::FRAC_PI_2)
                }
            };
            placement * local_open
        } else {
            glam::Mat4::from_translation(glam::Vec3::from(door.pos))
        };

        r.queue.write_buffer(&r.door_uniform_pool[slot].0, 0,
            bytemuck::bytes_of(&EntityUniform { model: mat.to_cols_array_2d(), tint: [1.0; 4] }));
        for mesh in model_meshes {
            // Resolve the mesh's decoded texture from the shared door texture set; the fallback
            // cube has texture_idx None -> white placeholder.
            let tex_bg = mesh.texture_idx.and_then(|i| r.door_textures.get(i));
            draws.push((slot, mesh, tex_bg));
        }
        slot += 1;
    }
    if draws.is_empty() { return; }

    // Phase 2: one render pass, drawing every recorded door mesh.
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("doors"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.character);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    for (slot_idx, mesh, tex_bg) in draws {
        pass.set_bind_group(1, tex_bg.unwrap_or(&r.fallback_texture_bg), &[]);
        pass.set_bind_group(2, &r.door_uniform_pool[slot_idx].1, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
}

/// Billboard pass for NPC entities that have no 3D model. Skipped if nothing to draw.
pub fn encode_billboard_pass(
    r:         &EqRenderer,
    encoder:   &mut wgpu::CommandEncoder,
    view:      &wgpu::TextureView,
    scene:     &SceneState,
    cam_right: [f32; 3],
    cam_up:    [f32; 3],
) {
    use wgpu::util::DeviceExt;
    use crate::billboard::{billboard_quad, npc_color, npc_size};

    let mut all_verts: Vec<crate::gpu::Vertex> = Vec::new();
    let mut all_idxs:  Vec<u32>                = Vec::new();

    for b in &scene.billboards {
        if r.character_model_for(&b.race, b.gender).is_some() { continue; }
        let (verts, idxs) = billboard_quad(
            b.pos, npc_size(b.level), npc_color(b.is_target, b.dead, b.hp_pct),
            cam_right, cam_up,
        );
        let base = all_verts.len() as u32;
        all_verts.extend(verts);
        all_idxs.extend(idxs.iter().map(|i| i + base));
    }

    if all_verts.is_empty() { return; }

    let vbuf = r.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("billboard_verts"),
        contents: bytemuck::cast_slice(&all_verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let ibuf = r.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("billboard_idxs"),
        contents: bytemuck::cast_slice(&all_idxs),
        usage: wgpu::BufferUsages::INDEX,
    });
    let idx_count = all_idxs.len() as u32;

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("billboards"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.billboard);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_vertex_buffer(0, vbuf.slice(..));
    pass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
    pass.draw_indexed(0..idx_count, 0, 0..1);
}

/// Player pass. Renders a 3D model when scene.player_race maps to a loaded archetype.
/// Draws nothing if no race is set or no model is loaded.
///
/// Uses entity_uniform_pool[0..PLAYER_UNIFORM_SLOTS) and joint_buf_pool[0] (player slot).
/// The entity passes must use pool slots >= PLAYER_UNIFORM_SLOTS to avoid overlap.
pub fn encode_player_pass(
    r:         &EqRenderer,
    encoder:   &mut wgpu::CommandEncoder,
    view:      &wgpu::TextureView,
    scene:     &SceneState,
) {
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::{race_to_archetype, archetype_scale};
    use crate::gpu::{EntityUniform, GpuModel};

    if !scene.player_race.is_empty() {
        let archetype = race_to_archetype(&scene.player_race);

        match r.character_model_for(&scene.player_race, scene.player_gender) {
            Some(GpuModel::Skinned(model)) => {
                let matrices = match r.anim_states.get(&0) {
                    Some(state) if !model.skin.clips.is_empty() =>
                        model.skin.evaluate(state.clip_idx, state.time),
                    _ => model.skin.bind_pose(),
                };
                let id4 = [[1f32,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];
                let mut joint_array = [id4; 128];
                for (i, m) in matrices.iter().enumerate().take(128) { joint_array[i] = *m; }
                // Write to pool slot 0 (reserved for player).
                r.queue.write_buffer(&r.joint_buf_pool[0].0, 0, bytemuck::cast_slice(&joint_array));

                let target = crate::models::target_height_for(&scene.player_race, archetype);
                // Normalize to `target` height and ground by the model's own feet. This math
                // lives in `models::humanoid_placement` so the placement regression test can
                // exercise the exact production computation (see the fn's doc; #357).
                //   - mesh_scale = target/true_height (NO node_scale re-apply, #149).
                //   - Skinned EQ models are authored horizontally centered on the origin, so NO
                //     recenter (center_xz=[0,0]); measured centers were unreliable and pushed
                //     the model off. Vertically the origin sits above the feet, so visual_scale
                //     lifts by -2*feet_offset*mesh_scale to ground the feet.
                let placement = crate::models::humanoid_placement(
                    model.true_height, model.feet_offset, target);
                let dominant_mesh_scale = placement.mesh_scale;
                let visual_scale = placement.visual_scale;

                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    let mat = crate::camera::entity_model_matrix_heading(
                        scene.player_pos, scene.player_heading, visual_scale,
                        dominant_mesh_scale, [0.0, 0.0], true, 0.0,
                        crate::models::archetype_correction(archetype),
                    );
                    let tint = match model.equip_slots[i] {
                        Some(ref es) if scene.player_equipment_tint[es.slot] != [0, 0, 0] => {
                            let t = scene.player_equipment_tint[es.slot];
                            [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                        }
                        _ => mesh.base_color,
                    };
                    // Runtime-tint synthetic hair shells by the player's hair color (eqoxide#98).
                    let tint = crate::models::head_part_tint(model.head_parts[i], scene.player_haircolor,
                        &scene.player_race, scene.player_gender)
                        .unwrap_or(tint);
                    r.queue.write_buffer(
                        &r.entity_uniform_pool[i].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
                    );
                }

                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("player_skinned"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view, resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &r.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None, occlusion_query_set: None,
                });
                pass.set_pipeline(&r.pipelines.skinned);
                pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
                pass.set_bind_group(3, &r.joint_buf_pool[0].1, &[]);
                // Two-layer Luclin body: pass 1 draws the opaque skin base (the model's baked
                // texture) for every visible mesh; pass 2 composites the cloth/armor overlay on top
                // (alpha-blended, LessEqual depth) so exposed skin shows where the overlay is
                // transparent (e.g. the elf-female midriff). See docs/equipment-textures-findings.md.
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    if equip_mesh_hidden(r, &model.prefix, model.equip_slots[i], &scene.player_equipment) { continue; }
                    if !crate::models::head_part_visible(
                        model.head_parts[i], model.head_default_hidden[i],
                        scene.player_face, scene.player_hairstyle,
                    ) { continue; }
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
                    pass.set_bind_group(1, skin_base_tex(r, &model.texture_bind_groups, mesh.texture_idx), &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
                pass.set_pipeline(&r.pipelines.skinned_overlay);
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    if equip_mesh_hidden(r, &model.prefix, model.equip_slots[i], &scene.player_equipment) { continue; }
                    if !crate::models::head_part_visible(
                        model.head_parts[i], model.head_default_hidden[i],
                        scene.player_face, scene.player_hairstyle,
                    ) { continue; }
                    let Some(overlay) = resolve_overlay_tex(r, &model.prefix,
                        model.equip_slots[i].clone(), &scene.player_equipment) else { continue };
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
                    pass.set_bind_group(1, overlay, &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
                drop(pass); // end the skinned pass before drawing the weapon

                // ── Held items: draw each equipped item at the rig's attachment bone
                // (R_POINT = primary hand, L_POINT = left hand, SHIELD_POINT = off-hand
                // shield), posed by the current animation so it swings with combat. EQ
                // authors IT models in the point-bone frame with an identity attach; the
                // only extra transform bridges the weapon bake's vertex convention into
                // the rig frame (see models::held_item_xform); no per-weapon tuning.
                let (clip_i, t) = r.anim_states.get(&0).map(|s| (s.clip_idx, s.time)).unwrap_or((0, 0.0));
                let pmat = glam::Mat4::from_cols_array_2d(&crate::camera::entity_model_matrix_heading(
                    scene.player_pos, scene.player_heading, visual_scale, dominant_mesh_scale,
                    [0.0, 0.0], true, 0.0, crate::models::archetype_correction(archetype)));
                let hx = crate::models::held_item_xform();
                let held = [
                    (scene.primary_weapon_idfile.to_uppercase(), "R_POINT", 0usize),
                    (scene.secondary_weapon_idfile.to_uppercase(),
                     crate::models::secondary_attach_bone(&scene.secondary_weapon_idfile), 1usize),
                ];
                let mut weapon_draws: Vec<(&crate::gpu::GpuWeapon, usize)> = Vec::new();
                for (wkey, bone, wslot) in &held {
                    let Some(Some(weapon)) = r.weapon_cache.get(wkey) else { continue };
                    // GLBs baked before joint names were exported can't locate the bone;
                    // skip rather than guess (a wrong bone reads worse than no weapon).
                    // Old rigs may lack SHIELD_POINT — fall back to gripping at L_POINT.
                    let Some(joint) = model.skin.attach_joint(bone)
                        .or_else(|| (*bone == "SHIELD_POINT")
                            .then(|| model.skin.attach_joint("L_POINT")).flatten())
                        else { continue };
                    let hand = glam::Mat4::from_cols_array_2d(&model.skin.joint_world(clip_i, t, joint));
                    let wmat = (pmat * hand * hx).to_cols_array_2d();
                    r.queue.write_buffer(&r.weapon_uniform_pool[*wslot].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: wmat, tint: [1.0, 1.0, 1.0, 1.0] }));
                    weapon_draws.push((weapon, *wslot));
                }
                if !weapon_draws.is_empty() {
                    let mut wpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                        label: Some("player_weapon"),
                        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                            view, resolve_target: None,
                            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                        })],
                        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                            view: &r.depth_view,
                            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                            stencil_ops: None,
                        }),
                        timestamp_writes: None, occlusion_query_set: None,
                    });
                    wpass.set_pipeline(&r.pipelines.character);
                    wpass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                    for (weapon, wslot) in &weapon_draws {
                        wpass.set_bind_group(2, &r.weapon_uniform_pool[*wslot].1, &[]);
                        for mesh in &weapon.meshes {
                            let bg = mesh.texture_idx.and_then(|ti| weapon.texture_bind_groups.get(ti))
                                .unwrap_or(&r.fallback_texture_bg);
                            wpass.set_bind_group(1, bg, &[]);
                            wpass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                            wpass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                            wpass.draw_indexed(0..mesh.index_count, 0, 0..1);
                        }
                    }
                }
                return;
            }
            Some(GpuModel::Static(model)) => {
                let arch_scale = archetype_scale(archetype);
                let visual_scale = 2.0 * model.y_extent * arch_scale;
                let mat = crate::camera::entity_model_matrix_heading(
                    scene.player_pos, scene.player_heading, visual_scale, arch_scale,
                    [model.x_center, model.z_center], true, model.y_bottom,
                    crate::models::archetype_correction(archetype),
                );
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    let tint = match model.equip_slots[i] {
                        Some(ref es) if scene.player_equipment_tint[es.slot] != [0, 0, 0] => {
                            let t = scene.player_equipment_tint[es.slot];
                            [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                        }
                        _ => mesh.base_color,
                    };
                    // Runtime-tint synthetic hair shells by the player's hair color (eqoxide#98).
                    let tint = crate::models::head_part_tint(model.head_parts[i], scene.player_haircolor,
                        &scene.player_race, scene.player_gender)
                        .unwrap_or(tint);
                    r.queue.write_buffer(
                        &r.entity_uniform_pool[i].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
                    );
                }
                let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                    label: Some("player_static"),
                    color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                        view, resolve_target: None,
                        ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                    })],
                    depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                        view: &r.depth_view,
                        depth_ops: Some(wgpu::Operations {
                            load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
                        }),
                        stencil_ops: None,
                    }),
                    timestamp_writes: None, occlusion_query_set: None,
                });
                pass.set_pipeline(&r.pipelines.character);
                pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
                pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
                    let bg = resolve_equip_tex(r, &model.texture_bind_groups, mesh.texture_idx,
                        &model.prefix, model.equip_slots[i].clone(), &scene.player_equipment);
                    pass.set_bind_group(1, bg, &[]);
                    pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
                    pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..mesh.index_count, 0, 0..1);
                }
                return;
            }
            None => {}
        }
    }
}

/// Render a single static model with the given transform.
/// This is the core rendering logic shared by the player pass, entity pass,
/// and the standalone model viewer (`render_model`).
///
/// `model_matrix` is the full 4×4 model→world transform (from `entity_model_matrix_heading`).
/// Uniform buffer slots are taken from `r.entity_uniform_pool[base_slot..]`.
/// At most `max_meshes` meshes are drawn; pass `usize::MAX` for no limit.
#[allow(clippy::too_many_arguments)]
pub fn render_static_model(
    r:            &EqRenderer,
    encoder:      &mut wgpu::CommandEncoder,
    view:         &wgpu::TextureView,
    model:        &crate::gpu::GpuStaticModel,
    model_matrix: [[f32; 4]; 4],
    tint:         [f32; 4],
    base_slot:    usize,
    max_meshes:   usize,
) {
    use crate::gpu::EntityUniform;

    let slot_count = r.entity_uniform_pool.len();
    for (i, _mesh) in model.meshes.iter().enumerate() {
        if i >= max_meshes { break; }
        let slot = base_slot + i;
        if slot >= slot_count { break; }
        r.queue.write_buffer(
            &r.entity_uniform_pool[slot].0, 0,
            bytemuck::bytes_of(&EntityUniform { model: model_matrix, tint }),
        );
    }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("static_model"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.character);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
    let mut cur_tex: Option<usize> = None;
    for (i, mesh) in model.meshes.iter().enumerate() {
        if i >= max_meshes { break; }
        let slot = base_slot + i;
        if slot >= slot_count { break; }
        pass.set_bind_group(2, &r.entity_uniform_pool[slot].1, &[]);
        if mesh.texture_idx != cur_tex {
            cur_tex = mesh.texture_idx;
            let bg = match cur_tex {
                Some(idx) if idx < model.texture_bind_groups.len() =>
                    &model.texture_bind_groups[idx],
                _ => &r.fallback_texture_bg,
            };
            pass.set_bind_group(1, bg, &[]);
        }
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
}

/// Static glTF character model pass — all static-model entities in ONE render pass.
/// Uses entity_uniform_pool[PLAYER_UNIFORM_SLOTS .. pool_len/2+PLAYER_UNIFORM_SLOTS).
pub fn encode_entity_pass(
    r:        &EqRenderer,
    encoder:  &mut wgpu::CommandEncoder,
    view:     &wgpu::TextureView,
    scene:    &SceneState,
    _cam_pos: [f32; 3],
) {
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::{race_to_archetype, archetype_scale};
    use crate::gpu::GpuModel;

    struct DrawCmd { archetype: &'static str, mesh_idx: usize, uniform_slot: usize, equipment: [u32; 9], gender: u8, face: u8, hairstyle: u8 }

    let mut draws: Vec<DrawCmd> = Vec::new();
    let pool_half = r.entity_uniform_pool.len() / 2;
    let slot_end  = PLAYER_UNIFORM_SLOTS + pool_half;
    let mut slot  = PLAYER_UNIFORM_SLOTS;

    for b in &scene.billboards {
        if !crate::camera::entity_in_view(b.pos, scene.player_pos, r.last_view_proj,
                                          ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN) { continue; }
        let archetype = race_to_archetype(&b.race);
        let Some(GpuModel::Static(model)) = r.model_for(archetype, b.gender) else { continue };
        let arch_scale   = archetype_scale(archetype);
        let visual_scale = 2.0 * model.y_extent * arch_scale;
        let mat = crate::camera::entity_model_matrix_heading(b.pos, b.heading, visual_scale, arch_scale,
            [model.x_center, model.z_center], true, model.y_bottom, crate::models::archetype_correction(archetype));
        for (mesh_idx, mesh) in model.meshes.iter().enumerate() {
            if slot >= slot_end { break; }
            let slot_meta = model.equip_slots[mesh_idx];
            let tint: [f32; 4] = if b.dead { [0.5, 0.5, 0.5, 1.0] }
                                 else if b.is_target { [1.0, 0.3, 0.3, 1.0] }
                                 else {
                                     match slot_meta {
                                         Some(es) if b.equipment_tint[es.slot] != [0, 0, 0] => {
                                             let t = b.equipment_tint[es.slot];
                                             [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                                         }
                                         _ => mesh.base_color,
                                     }
                                 };
            // Runtime-tint synthetic hair shells by the NPC's hair color (eqoxide#98) — unless the
            // whole model is dead-greyed or target-highlighted (those overrides win).
            let tint = if !b.dead && !b.is_target {
                crate::models::head_part_tint(model.head_parts[mesh_idx], b.haircolor, &b.race, b.gender).unwrap_or(tint)
            } else { tint };
            r.queue.write_buffer(
                &r.entity_uniform_pool[slot].0, 0,
                bytemuck::bytes_of(&crate::gpu::EntityUniform { model: mat, tint }),
            );
            draws.push(DrawCmd { archetype, mesh_idx, uniform_slot: slot, equipment: b.equipment, gender: b.gender, face: b.face, hairstyle: b.hairstyle });
            slot += 1;
        }
        if slot >= slot_end { break; }
    }
    if draws.is_empty() { return; }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("entities"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.character);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);

    for draw in &draws {
        let Some(GpuModel::Static(model)) = r.model_for(draw.archetype, draw.gender) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if equip_mesh_hidden(r, &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment) { continue; }
        if !crate::models::head_part_visible(
            model.head_parts[draw.mesh_idx],
            model.head_default_hidden[draw.mesh_idx],
            draw.face, draw.hairstyle,
        ) { continue; }
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        let bg = resolve_equip_tex(r, &model.texture_bind_groups, mesh.texture_idx,
            &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment);
        pass.set_bind_group(1, bg, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
}

/// Skinned glTF character model pass — all skinned-model entities in ONE render pass.
/// Joint pool: slot 0 = player (reserved), slots 1..N = entities.
/// Uniform pool: upper half (avoids overlap with static entity pass and player slots).
pub fn encode_skinned_entity_pass(
    r:        &EqRenderer,
    encoder:  &mut wgpu::CommandEncoder,
    view:     &wgpu::TextureView,
    scene:    &SceneState,
    _cam_pos: [f32; 3],
) {
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::race_to_archetype;
    use crate::gpu::{EntityUniform, GpuModel};

    struct DrawCmd { model_key: &'static str, model_slot: u8, mesh_idx: usize, uniform_slot: usize, joint_slot: usize, equipment: [u32; 9], face: u8, hairstyle: u8 }

    let mut draws: Vec<DrawCmd> = Vec::new();
    // Held items (spawn equipment slots 7/8) drawn at the rig's attach bones, same
    // contract as the player pass. weapon_uniform_pool slots 0-1 belong to the player;
    // entities allocate from 2, nearest-first, and overflow just skips the item.
    let mut weapon_draws: Vec<(String, usize)> = Vec::new();
    let mut w_slot = 2usize;
    let hx = crate::models::held_item_xform();
    let pool_half    = r.entity_uniform_pool.len() / 2;
    let uniform_base = pool_half + PLAYER_UNIFORM_SLOTS; // upper half for skinned
    let mut u_slot   = uniform_base;
    let mut j_slot   = 1usize; // slot 0 reserved for player

    let id4 = [[1f32,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];

    // Each humanoid model is ~27 meshes, so the uniform/joint pools can't hold every
    // spawn in a crowded zone. Render NEAREST-first so the NPCs around the player always
    // draw, and only draw a model that fits ENTIRELY in the remaining pool (no partial,
    // shrunken-looking bodies). Distant overflow spawns fall back to their nameplate.
    let pp = scene.player_pos;
    let mut order: Vec<&crate::scene::Billboard> =
        scene.billboards.iter().collect();
    order.sort_by(|a, b| {
        let da = (a.pos[0]-pp[0]).powi(2) + (a.pos[1]-pp[1]).powi(2) + (a.pos[2]-pp[2]).powi(2);
        let db = (b.pos[0]-pp[0]).powi(2) + (b.pos[1]-pp[1]).powi(2) + (b.pos[2]-pp[2]).powi(2);
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });

    for b in order {
        if !crate::camera::entity_in_view(b.pos, scene.player_pos, r.last_view_proj,
                                          ENTITY_DRAW_DIST, ENTITY_CULL_MARGIN) { continue; }
        let archetype = race_to_archetype(&b.race);
        let (model_key, model_slot) = crate::models::character_model_key(&b.race, b.gender);
        let Some(GpuModel::Skinned(model)) = r.model_by_key(model_key, model_slot) else { continue };
        // Skip (don't break) if this model doesn't fully fit — a later, smaller model
        // (e.g. an 8-mesh rat) may still fit in the remaining slots.
        if j_slot >= r.joint_buf_pool.len() { continue; }
        if u_slot + model.meshes.len() > r.entity_uniform_pool.len() { continue; }

        // Use the animation state's clip and time for all actions including "dead":
        // the renderer plays the D05 death clip once and holds the last frame.
        // When no death clip exists the sentinel clip_idx (usize::MAX) is out of range
        // so the bind-pose fallback fires automatically — standing corpse as before.
        let matrices: Vec<[[f32;4];4]> = match r.anim_states.get(&b.id) {
            Some(state) if !model.skin.clips.is_empty()
                        && state.clip_idx < model.skin.clips.len() =>
                model.skin.evaluate(state.clip_idx, state.time),
            _ => model.skin.bind_pose(),
        };
        let mut joint_array = [id4; 128];
        for (i, m) in matrices.iter().enumerate().take(128) { joint_array[i] = *m; }
        r.queue.write_buffer(&r.joint_buf_pool[j_slot].0, 0, bytemuck::cast_slice(&joint_array));

        let target = crate::models::target_height_for(&b.race, archetype);
        let height = if model.true_height > 0.001 { model.true_height } else { 1.0 };
        // See the player pass: normalize to `target` only — do not re-apply the authored
        // `node_scale` (it would re-inflate; the scale-100 `fish.glb` rendered ~100× too big).
        let dominant_scale    = target / height;
        // Same placement as the player pass: no recenter (models are authored centered),
        // lift by a calibrated fraction of target height to ground the feet.
        // Ground by the model's own feet: lift = -feet_offset * mesh_scale.
        let visual_scale = -2.0 * model.feet_offset * dominant_scale;

        for (mesh_idx, mesh) in model.meshes.iter().enumerate() {
            let mat = crate::camera::entity_model_matrix_heading(
                b.pos, b.heading, visual_scale, dominant_scale,
                [0.0, 0.0], true, 0.0,
                crate::models::archetype_correction(archetype),
            );
            let slot_meta = model.equip_slots[mesh_idx];
            let tint: [f32; 4] = if b.dead { [0.5, 0.5, 0.5, 1.0] }
                                 else if b.is_target { [1.0, 0.3, 0.3, 1.0] }
                                 else {
                                     match slot_meta {
                                         Some(es) if b.equipment_tint[es.slot] != [0, 0, 0] => {
                                             let t = b.equipment_tint[es.slot];
                                             [t[0] as f32 / 255.0, t[1] as f32 / 255.0, t[2] as f32 / 255.0, 1.0]
                                         }
                                         _ => mesh.base_color,
                                     }
                                 };
            // Runtime-tint synthetic hair shells by the NPC's hair color (eqoxide#98) — unless the
            // whole model is dead-greyed or target-highlighted (those overrides win).
            let tint = if !b.dead && !b.is_target {
                crate::models::head_part_tint(model.head_parts[mesh_idx], b.haircolor, &b.race, b.gender).unwrap_or(tint)
            } else { tint };
            r.queue.write_buffer(
                &r.entity_uniform_pool[u_slot].0, 0,
                bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
            );
            draws.push(DrawCmd { model_key, model_slot, mesh_idx, uniform_slot: u_slot, joint_slot: j_slot, equipment: b.equipment, face: b.face, hairstyle: b.hairstyle });
            u_slot += 1;
        }

        // Held items at the rig attach bones, posed to match the body (animated pose
        // when the anim state is valid, rest pose when the body fell back to bind).
        let emat = glam::Mat4::from_cols_array_2d(&crate::camera::entity_model_matrix_heading(
            b.pos, b.heading, visual_scale, dominant_scale, [0.0, 0.0], true, 0.0,
            crate::models::archetype_correction(archetype)));
        let anim = r.anim_states.get(&b.id)
            .filter(|s| !model.skin.clips.is_empty() && s.clip_idx < model.skin.clips.len());
        for held in crate::models::held_item_keys(&b.equipment, b.dead) {
            let Some((key, bone)) = held else { continue };
            if w_slot >= r.weapon_uniform_pool.len() { break; }
            if !matches!(r.weapon_cache.get(&key), Some(Some(_))) { continue; }
            // Old rigs may lack SHIELD_POINT — fall back to gripping at L_POINT.
            let Some(joint) = model.skin.attach_joint(bone)
                .or_else(|| (bone == "SHIELD_POINT").then(|| model.skin.attach_joint("L_POINT")).flatten())
                else { continue };
            let hand = glam::Mat4::from_cols_array_2d(&match anim {
                Some(s) => model.skin.joint_world(s.clip_idx, s.time, joint),
                None    => model.skin.joint_world_rest(joint),
            });
            let wmat = (emat * hand * hx).to_cols_array_2d();
            r.queue.write_buffer(&r.weapon_uniform_pool[w_slot].0, 0,
                bytemuck::bytes_of(&EntityUniform { model: wmat, tint: [1.0, 1.0, 1.0, 1.0] }));
            weapon_draws.push((key, w_slot));
            w_slot += 1;
        }
        j_slot += 1;
    }
    if draws.is_empty() { return; }

    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("skinned_entities"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view, resolve_target: None,
            ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: &r.depth_view,
            depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
            stencil_ops: None,
        }),
        timestamp_writes: None, occlusion_query_set: None,
    });
    pass.set_pipeline(&r.pipelines.skinned);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);

    // Two-layer Luclin body (same as the player pass): pass 1 lays down the opaque skin base for
    // every visible mesh; pass 2 composites the cloth/armor overlay on top, so skin shows through
    // wherever the overlay is transparent.
    let mut cur_joint = usize::MAX;
    for draw in &draws {
        let Some(GpuModel::Skinned(model)) = r.model_by_key(draw.model_key, draw.model_slot) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if draw.joint_slot != cur_joint {
            pass.set_bind_group(3, &r.joint_buf_pool[draw.joint_slot].1, &[]);
            cur_joint = draw.joint_slot;
        }
        if equip_mesh_hidden(r, &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment) { continue; }
        if !crate::models::head_part_visible(
            model.head_parts[draw.mesh_idx],
            model.head_default_hidden[draw.mesh_idx],
            draw.face, draw.hairstyle,
        ) { continue; }
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        pass.set_bind_group(1, skin_base_tex(r, &model.texture_bind_groups, mesh.texture_idx), &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }
    pass.set_pipeline(&r.pipelines.skinned_overlay);
    cur_joint = usize::MAX;
    for draw in &draws {
        let Some(GpuModel::Skinned(model)) = r.model_by_key(draw.model_key, draw.model_slot) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if draw.joint_slot != cur_joint {
            pass.set_bind_group(3, &r.joint_buf_pool[draw.joint_slot].1, &[]);
            cur_joint = draw.joint_slot;
        }
        if equip_mesh_hidden(r, &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment) { continue; }
        if !crate::models::head_part_visible(
            model.head_parts[draw.mesh_idx],
            model.head_default_hidden[draw.mesh_idx],
            draw.face, draw.hairstyle,
        ) { continue; }
        let Some(overlay) = resolve_overlay_tex(r, &model.prefix,
            model.equip_slots[draw.mesh_idx], &draw.equipment) else { continue };
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        pass.set_bind_group(1, overlay, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
    }

    // Held items: static meshes at the pre-posed attach-bone matrices (same pipeline
    // the player weapon pass uses; bind group 3 is beyond this pipeline's layout and
    // is ignored).
    pass.set_pipeline(&r.pipelines.character);
    for (key, wslot) in &weapon_draws {
        let Some(Some(weapon)) = r.weapon_cache.get(key) else { continue };
        pass.set_bind_group(2, &r.weapon_uniform_pool[*wslot].1, &[]);
        for mesh in &weapon.meshes {
            let bg = mesh.texture_idx.and_then(|ti| weapon.texture_bind_groups.get(ti))
                .unwrap_or(&r.fallback_texture_bg);
            pass.set_bind_group(1, bg, &[]);
            pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
            pass.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_sky_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
        ) = encode_sky_pass;
    }

    #[test]
    fn encode_zone_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
        ) = encode_zone_pass;
    }

    #[test]
    fn encode_billboard_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
            [f32; 3],
            [f32; 3],
        ) = encode_billboard_pass;
    }

    #[test]
    fn encode_player_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
        ) = encode_player_pass;
    }

    #[test]
    fn encode_entity_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
            [f32; 3],
        ) = encode_entity_pass;
    }

    #[test]
    fn encode_skinned_entity_pass_has_correct_signature() {
        let _: fn(
            &crate::renderer::EqRenderer,
            &mut wgpu::CommandEncoder,
            &wgpu::TextureView,
            &crate::scene::SceneState,
            [f32; 3],
        ) = encode_skinned_entity_pass;
    }
}
