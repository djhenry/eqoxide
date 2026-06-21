//! Low-level wgpu building blocks: vertex formats (`Vertex`, `SkinnedVertex`), GPU-side mesh/model/
//! texture wrappers, and helpers to upload textures, build the depth buffer, and create bind groups.

use crate::assets::TextureData;

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub position: [f32; 3],
    pub normal:   [f32; 3],
    pub uv:       [f32; 2],
}

#[allow(dead_code)] // fields kept alive for RAII; wgpu bind groups hold their own references
pub struct GpuTexture {
    pub view:    wgpu::TextureView,
    pub sampler: wgpu::Sampler,
}

pub struct GpuMesh {
    pub vertex_buf:  wgpu::Buffer,
    pub index_buf:   wgpu::Buffer,
    pub index_count: u32,
    pub texture_idx: Option<usize>,
    pub base_color:  [f32; 4],
}

/// Skinned mesh vertex — 64 bytes, Pod + Zeroable.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct SkinnedVertex {
    pub position:      [f32; 3],   // 12 bytes
    pub normal:        [f32; 3],   // 12 bytes
    pub uv:            [f32; 2],   //  8 bytes
    pub joint_indices: [u32; 4],   // 16 bytes
    pub joint_weights: [f32; 4],   // 16 bytes
}

pub struct GpuSkinnedMesh {
    pub vertex_buf:      wgpu::Buffer,  // holds SkinnedVertex
    pub index_buf:       wgpu::Buffer,
    pub index_count:     u32,
    pub texture_idx:     Option<usize>,
    pub base_color:      [f32; 4],
    /// This mesh's node_scale from the glTF scene graph (may differ from the dominant scale
    /// stored on the model, e.g. weapon accessories vs the body mesh).
    #[allow(dead_code)]
    pub mesh_node_scale: f32,
}

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct EntityUniform {
    pub model: [[f32; 4]; 4],
    pub tint:  [f32; 4],
}

/// Static (non-animated) character model.
pub struct GpuStaticModel {
    pub meshes:              Vec<GpuMesh>,
    pub texture_bind_groups: Vec<wgpu::BindGroup>,
    /// Distance from Y=0 to the bottom of the model in buffer vertex space.
    /// Used to compute the ground lift so models stand at Z=0 instead of floating or sinking.
    pub y_bottom:            f32,
    /// Vertical extent of the model (max_y - min_y) in buffer vertex space.
    /// Used to compute visual_scale: visual_scale = 2 * y_extent * arch_scale.
    /// Separate from y_bottom because chr.s3d models may have vertices far above Y=0
    /// (e.g. feet at Y=20), making y_bottom unreliable as a height proxy.
    pub y_extent:            f32,
    pub x_center:            f32,
    pub z_center:            f32,
    /// Lowercase race+gender prefix from material names; empty if unknown.
    pub prefix: String,
    /// Per-mesh equipment slot binding, parallel to `meshes`. `None` = not an armor slot.
    pub equip_slots: Vec<Option<crate::models::EquipSlot>>,
    /// True model height in EQ units. From `eq_height` glTF extras if present; otherwise
    /// the measured `y_extent`. Use this for scale calculations (Task 4).
    pub true_height: f32,
    /// Per-clip posed bounds (center_x, center_z, feet_floor), parallel to `skin.clips`.
    /// Empty for static models. Recenter + ground from the current clip vs the bind pose.
    pub clip_bounds: Vec<(f32, f32, f32)>,
    /// Robust feet height (idle-pose model-Y 5th percentile); 0 for static. Grounding lifts
    /// by `-feet_offset × mesh_scale` so each archetype sits on the floor by its own feet.
    pub feet_offset: f32,
}

/// Skinned (GPU-animated) character model with embedded SkinData.
pub struct GpuSkinnedModel {
    pub meshes:              Vec<GpuSkinnedMesh>,
    pub texture_bind_groups: Vec<wgpu::BindGroup>,
    pub skin:                crate::anim::SkinData,
    /// Scene-graph node scale from the glTF document (e.g. 100.0 for Quaternius/CC0 models).
    /// Not baked into vertices (that would break joint matrices), so we apply it to the
    /// entity model matrix in the render pass instead.
    pub node_scale:          f32,
    /// Distance from Y=0 to the bottom of the model in raw (pre-node-scale) vertex space.
    /// Used to compute the ground lift: lift = y_bottom × mesh_scale.
    #[allow(dead_code)]
    pub y_bottom:            f32,
    /// Center of the model in X and Z axes (raw pre-node-scale space, dominant-scale meshes).
    /// Applied as a centering correction so models render at their entity position, not offset.
    pub x_center:            f32,
    pub z_center:            f32,
    /// Lowercase race+gender prefix from material names; empty if unknown.
    pub prefix: String,
    /// Per-mesh equipment slot binding, parallel to `meshes`. `None` = not an armor slot.
    pub equip_slots: Vec<Option<crate::models::EquipSlot>>,
    /// True model height in EQ units. From `eq_height` glTF extras if present; otherwise
    /// the measured `y_extent`. Use this for scale calculations (Task 4).
    pub true_height: f32,
    /// Per-clip posed bounds (center_x, center_z, feet_floor), parallel to `skin.clips`.
    /// Empty for static models. Recenter + ground from the current clip vs the bind pose.
    pub clip_bounds: Vec<(f32, f32, f32)>,
    /// Robust feet height (idle-pose model-Y 5th percentile); 0 for static. Grounding lifts
    /// by `-feet_offset × mesh_scale` so each archetype sits on the floor by its own feet.
    pub feet_offset: f32,
}

/// Unified character model — either static or skinned.
pub enum GpuModel {
    Static(GpuStaticModel),
    Skinned(GpuSkinnedModel),
}

/// Upload CPU textures to GPU. Returns parallel (gpu_textures, bind_groups) vecs.
pub fn upload_textures(
    device:   &wgpu::Device,
    queue:    &wgpu::Queue,
    textures: &[TextureData],
    bgl:      &wgpu::BindGroupLayout,
) -> (Vec<GpuTexture>, Vec<wgpu::BindGroup>) {
    let mut gpu_textures = Vec::with_capacity(textures.len());
    let mut bind_groups  = Vec::with_capacity(textures.len());

    for tex in textures {
        let size = wgpu::Extent3d { width: tex.width, height: tex.height, depth_or_array_layers: 1 };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some(&tex.name),
            size,
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            texture.as_image_copy(),
            &tex.rgba,
            wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(4 * tex.width),
                rows_per_image: Some(tex.height),
            },
            size,
        );
        let view    = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("zone_sampler"),
            address_mode_u: wgpu::AddressMode::Repeat,
            address_mode_v: wgpu::AddressMode::Repeat,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("texture_bg"),
            layout: bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        gpu_textures.push(GpuTexture { view, sampler });
        bind_groups.push(bg);
    }

    (gpu_textures, bind_groups)
}

/// Create a Depth32Float texture view for the given dimensions.
/// Call once at startup and again on resize.
pub fn create_depth_texture(device: &wgpu::Device, width: u32, height: u32) -> wgpu::TextureView {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Depth32Float,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    }).create_view(&wgpu::TextureViewDescriptor::default())
}

/// 1×1 white fallback bind group for untextured meshes.
pub fn build_fallback_texture_bg(
    device: &wgpu::Device,
    queue:  &wgpu::Queue,
    bgl:    &wgpu::BindGroupLayout,
) -> wgpu::BindGroup {
    let tex = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("white_fallback"),
        size: wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        mip_level_count: 1, sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8UnormSrgb,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        tex.as_image_copy(),
        &[255u8, 255, 255, 255],
        wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(4), rows_per_image: Some(1) },
        wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
    );
    let view    = tex.create_view(&wgpu::TextureViewDescriptor::default());
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        mag_filter: wgpu::FilterMode::Nearest,
        min_filter: wgpu::FilterMode::Nearest,
        ..Default::default()
    });
    device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("fallback_bg"),
        layout: bgl,
        entries: &[
            wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&view) },
            wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_is_pod() {
        // Vertex must implement Pod + Zeroable for bytemuck::cast_slice.
        fn _check<T: bytemuck::Pod + bytemuck::Zeroable>() {}
        _check::<Vertex>();
    }

    #[test]
    fn entity_uniform_is_pod() {
        fn _check<T: bytemuck::Pod + bytemuck::Zeroable>() {}
        _check::<EntityUniform>();
    }

    #[test]
    fn gpu_mesh_texture_idx_is_option_usize() {
        fn _check(m: &GpuMesh) -> Option<usize> { m.texture_idx }
        // Compiles only if the field exists with the right type.
    }

    #[test]
    fn texture_idx_sort_key_none_sorts_last() {
        let mut idxs: Vec<Option<usize>> = vec![None, Some(2), Some(0), Some(1), None];
        idxs.sort_by_key(|t| t.map_or(usize::MAX, |i| i));
        assert_eq!(idxs, vec![Some(0), Some(1), Some(2), None, None]);
    }

    #[test]
    fn create_depth_texture_has_correct_signature() {
        let _: fn(&wgpu::Device, u32, u32) -> wgpu::TextureView = create_depth_texture;
    }

    #[test]
    fn build_fallback_texture_bg_has_correct_signature() {
        let _: fn(&wgpu::Device, &wgpu::Queue, &wgpu::BindGroupLayout) -> wgpu::BindGroup =
            build_fallback_texture_bg;
    }

    #[test]
    fn skinned_vertex_is_pod() {
        fn _check<T: bytemuck::Pod + bytemuck::Zeroable>() {}
        _check::<SkinnedVertex>();
    }

    #[test]
    fn skinned_vertex_stride_is_64() {
        assert_eq!(std::mem::size_of::<SkinnedVertex>(), 64);
    }

    #[test]
    fn gpu_model_has_static_and_skinned_variants() {
        fn _check(m: GpuModel) {
            match m {
                GpuModel::Static(_)  => {}
                GpuModel::Skinned(_) => {}
            }
        }
    }
}
