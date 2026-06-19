use crate::renderer::EqRenderer;
use crate::scene::SceneState;

/// Choose the bind group for one primitive: an equipment-swapped armor texture if
/// available, else the primitive's baked GLB texture, else the white fallback.
fn resolve_equip_tex<'a>(
    r:          &'a EqRenderer,
    baked_bgs:  &'a [wgpu::BindGroup],
    baked_idx:  Option<usize>,
    prefix:     &str,
    slot:       Option<crate::models::EquipSlot>,
    equipment:  &[u32; 9],
) -> &'a wgpu::BindGroup {
    if let Some(es) = slot {
        if !prefix.is_empty() {
            let key = crate::models::equip_texture_name(prefix, &es.region, equipment[es.slot], es.variant);
            if let Some(Some(bg)) = r.equipment_tex_cache.get(&key) {
                return bg;
            }
        }
    }
    match baked_idx {
        Some(i) if i < baked_bgs.len() => &baked_bgs[i],
        _ => &r.fallback_texture_bg,
    }
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

    pass.set_pipeline(&r.pipelines.zone);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_bind_group(1, &r.fallback_texture_bg, &[]);
    let mut current_tex: Option<usize> = None;
    for mesh in &r.gpu_meshes {
        if mesh.texture_idx != current_tex {
            current_tex = mesh.texture_idx;
            let bg = match current_tex {
                Some(idx) => &r.texture_bind_groups[idx],
                None      => &r.fallback_texture_bg,
            };
            pass.set_bind_group(1, bg, &[]);
        }
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
    use crate::billboard::{billboard_quad, cross_marker, npc_color, npc_size};
    use crate::models::race_to_archetype;

    let mut all_verts: Vec<crate::gpu::Vertex> = Vec::new();
    let mut all_idxs:  Vec<u32>                = Vec::new();

    for b in &scene.billboards {
        if b.level == 0 {
            // Level-0 placeholder spawns: draw a small red X on the ground
            let (verts, idxs) = cross_marker(b.pos, 4.0, [0.9, 0.2, 0.2]);
            let base = all_verts.len() as u32;
            all_verts.extend(verts);
            all_idxs.extend(idxs.iter().map(|i| i + base));
            continue;
        }
        if r.gpu_character_models.contains_key(race_to_archetype(&b.race)) { continue; }
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

/// Player pass. Renders a 3D model when scene.player_race maps to a loaded archetype;
/// falls back to a blue billboard when no race is set or no model is loaded.
///
/// Uses entity_uniform_pool[0..PLAYER_UNIFORM_SLOTS) and joint_buf_pool[0] (player slot).
/// The entity passes must use pool slots >= PLAYER_UNIFORM_SLOTS to avoid overlap.
pub fn encode_player_pass(
    r:         &EqRenderer,
    encoder:   &mut wgpu::CommandEncoder,
    view:      &wgpu::TextureView,
    scene:     &SceneState,
    cam_right: [f32; 3],
    cam_up:    [f32; 3],
) {
    use wgpu::util::DeviceExt;
    use crate::renderer::PLAYER_UNIFORM_SLOTS;
    use crate::models::{race_to_archetype, archetype_scale};
    use crate::gpu::{EntityUniform, GpuModel};

    if !scene.player_race.is_empty() {
        let archetype = race_to_archetype(&scene.player_race);

        match r.gpu_character_models.get(archetype) {
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

                let arch_scale = archetype_scale(archetype);
                let dominant_mesh_scale = arch_scale * model.node_scale;
                let lift_basis = -model.skin.bind_lowest_skinned_z();
                let visual_scale = 2.0 * lift_basis * dominant_mesh_scale;
                let center_xz = [model.x_center, model.z_center];

                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    let mat = crate::camera::entity_model_matrix_heading(
                        scene.player_pos, scene.player_heading, visual_scale,
                        dominant_mesh_scale, center_xz, true, 0.0,
                    );
                    r.queue.write_buffer(
                        &r.entity_uniform_pool[i].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: mat, tint: mesh.base_color }),
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
                let mut cur_tex: Option<usize> = None;
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
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
                return;
            }
            Some(GpuModel::Static(model)) => {
                let arch_scale = archetype_scale(archetype);
                let visual_scale = 2.0 * model.y_extent * arch_scale;
                let mat = crate::camera::entity_model_matrix_heading(
                    scene.player_pos, scene.player_heading, visual_scale, arch_scale,
                    [model.x_center, model.z_center], true, model.y_bottom,
                );
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    r.queue.write_buffer(
                        &r.entity_uniform_pool[i].0, 0,
                        bytemuck::bytes_of(&EntityUniform { model: mat, tint: mesh.base_color }),
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
                let mut cur_tex: Option<usize> = None;
                for (i, mesh) in model.meshes.iter().enumerate() {
                    if i >= PLAYER_UNIFORM_SLOTS { break; }
                    pass.set_bind_group(2, &r.entity_uniform_pool[i].1, &[]);
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
                return;
            }
            None => {}
        }
    }

    // Fallback: blue billboard.
    use crate::billboard::billboard_quad;
    let (verts, idxs) = billboard_quad(
        scene.player_pos, 8.0, [0.34, 0.65, 1.0], cam_right, cam_up,
    );
    let vbuf = r.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("player_vbuf"),
        contents: bytemuck::cast_slice(&verts),
        usage: wgpu::BufferUsages::VERTEX,
    });
    let ibuf = r.device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("player_ibuf"),
        contents: bytemuck::cast_slice(&idxs),
        usage: wgpu::BufferUsages::INDEX,
    });
    let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("player"),
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
    pass.set_pipeline(&r.pipelines.billboard);
    pass.set_bind_group(0, &r.camera_uniform.bind_group, &[]);
    pass.set_vertex_buffer(0, vbuf.slice(..));
    pass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);
    pass.draw_indexed(0..6, 0, 0..1);
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

    struct DrawCmd { archetype: &'static str, mesh_idx: usize, uniform_slot: usize, equipment: [u32; 9] }

    let mut draws: Vec<DrawCmd> = Vec::new();
    let pool_half = r.entity_uniform_pool.len() / 2;
    let slot_end  = PLAYER_UNIFORM_SLOTS + pool_half;
    let mut slot  = PLAYER_UNIFORM_SLOTS;

    let mut debug_logged = false;
    let mut skipped = 0u32;
    let mut rendered = 0u32;
    for b in &scene.billboards {
        if b.level == 0 { continue; }
        let archetype = race_to_archetype(&b.race);
        let Some(GpuModel::Static(model)) = r.gpu_character_models.get(archetype) else { skipped += 1; continue };
        rendered += 1;
        let arch_scale   = archetype_scale(archetype);
        let visual_scale = 2.0 * model.y_extent * arch_scale;
        let lift = visual_scale * 0.5 + model.y_bottom * arch_scale;
        if !debug_logged {
            eprintln!("pass: billboard '{}' arch={} y_extent={:.4} y_bottom={:.4} arch_scale={:.2} visual_scale={:.4} lift={:.4} pos={:?}",
                b.race, archetype, model.y_extent, model.y_bottom, arch_scale, visual_scale, lift, b.pos);
            debug_logged = true;
        }
        let mat = crate::camera::entity_model_matrix_heading(b.pos, b.heading, visual_scale, arch_scale,
            [model.x_center, model.z_center], true, model.y_bottom);
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
            r.queue.write_buffer(
                &r.entity_uniform_pool[slot].0, 0,
                bytemuck::bytes_of(&crate::gpu::EntityUniform { model: mat, tint }),
            );
            draws.push(DrawCmd { archetype, mesh_idx, uniform_slot: slot, equipment: b.equipment });
            slot += 1;
        }
        if slot >= slot_end { break; }
    }
    eprintln!("pass: entity pass — {} draws, {} rendered, {} skipped (no model)", draws.len(), rendered, skipped);
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
        let Some(GpuModel::Static(model)) = r.gpu_character_models.get(draw.archetype) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
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
    use crate::models::{race_to_archetype, archetype_scale};
    use crate::gpu::{EntityUniform, GpuModel};

    struct DrawCmd { archetype: &'static str, mesh_idx: usize, uniform_slot: usize, joint_slot: usize, equipment: [u32; 9] }

    let mut draws: Vec<DrawCmd> = Vec::new();
    let pool_half    = r.entity_uniform_pool.len() / 2;
    let uniform_base = pool_half + PLAYER_UNIFORM_SLOTS; // upper half for skinned
    let mut u_slot   = uniform_base;
    let mut j_slot   = 1usize; // slot 0 reserved for player

    let id4 = [[1f32,0.,0.,0.],[0.,1.,0.,0.],[0.,0.,1.,0.],[0.,0.,0.,1.]];

    for b in &scene.billboards {
        if b.level == 0 { continue; }
        let archetype = race_to_archetype(&b.race);
        let Some(GpuModel::Skinned(model)) = r.gpu_character_models.get(archetype) else { continue };
        if j_slot >= r.joint_buf_pool.len() { break; }

        let matrices: Vec<[[f32;4];4]> = if b.action == "dead" {
            model.skin.bind_pose()
        } else {
            match r.anim_states.get(&b.id) {
                Some(state) if !model.skin.clips.is_empty() =>
                    model.skin.evaluate(state.clip_idx, state.time),
                _ => model.skin.bind_pose(),
            }
        };
        let mut joint_array = [id4; 128];
        for (i, m) in matrices.iter().enumerate().take(128) { joint_array[i] = *m; }
        r.queue.write_buffer(&r.joint_buf_pool[j_slot].0, 0, bytemuck::cast_slice(&joint_array));

        let arch_scale        = archetype_scale(archetype);
        let dominant_scale    = arch_scale * model.node_scale;
        let lift_basis = if b.action != "dead" {
            match r.anim_states.get(&b.id) {
                Some(state) if !model.skin.clips.is_empty() =>
                    -model.skin.lowest_skinned_z(state.clip_idx, state.time),
                _ => -model.skin.bind_lowest_skinned_z(),
            }
        } else {
            -model.skin.bind_lowest_skinned_z()
        };
        let visual_scale = 2.0 * lift_basis * dominant_scale;

        for (mesh_idx, mesh) in model.meshes.iter().enumerate() {
            if u_slot >= r.entity_uniform_pool.len() { break; }
            let mat = crate::camera::entity_model_matrix_heading(
                b.pos, b.heading, visual_scale, dominant_scale,
                [model.x_center, model.z_center], true, 0.0,
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
            r.queue.write_buffer(
                &r.entity_uniform_pool[u_slot].0, 0,
                bytemuck::bytes_of(&EntityUniform { model: mat, tint }),
            );
            draws.push(DrawCmd { archetype, mesh_idx, uniform_slot: u_slot, joint_slot: j_slot, equipment: b.equipment });
            u_slot += 1;
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

    let mut cur_joint = usize::MAX;
    for draw in &draws {
        let Some(GpuModel::Skinned(model)) = r.gpu_character_models.get(draw.archetype) else { continue };
        let mesh = &model.meshes[draw.mesh_idx];
        if draw.joint_slot != cur_joint {
            pass.set_bind_group(3, &r.joint_buf_pool[draw.joint_slot].1, &[]);
            cur_joint = draw.joint_slot;
        }
        pass.set_bind_group(2, &r.entity_uniform_pool[draw.uniform_slot].1, &[]);
        let bg = resolve_equip_tex(r, &model.texture_bind_groups, mesh.texture_idx,
            &model.prefix, model.equip_slots[draw.mesh_idx], &draw.equipment);
        pass.set_bind_group(1, bg, &[]);
        pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
        pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..mesh.index_count, 0, 0..1);
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
            [f32; 3],
            [f32; 3],
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
