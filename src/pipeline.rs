//! wgpu bind-group layouts (camera, texture, entity, joints), the camera uniform, and the render
//! pipelines + shaders for the zone, skinned-character, and billboard passes.

use crate::gpu::Vertex;

pub struct Layouts {
    pub camera_bgl:  wgpu::BindGroupLayout,
    pub texture_bgl: wgpu::BindGroupLayout,
    pub entity_bgl:  wgpu::BindGroupLayout,
    pub joints_bgl:  wgpu::BindGroupLayout,
}

pub struct CameraUniform {
    pub buf:        wgpu::Buffer,
    pub bind_group: wgpu::BindGroup,
}

pub struct Pipelines {
    pub sky:       wgpu::RenderPipeline,
    pub zone:      wgpu::RenderPipeline,
    pub billboard: wgpu::RenderPipeline,
    pub character: wgpu::RenderPipeline,
    pub skinned:   wgpu::RenderPipeline,
}

/// Create the three bind group layouts used across all pipelines.
pub fn build_layouts(device: &wgpu::Device) -> Layouts {
    let camera_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("camera_bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let texture_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("texture_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    });

    let entity_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("entity_bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    let joints_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("joints_bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::VERTEX,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    Layouts { camera_bgl, texture_bgl, entity_bgl, joints_bgl }
}

/// Create the camera uniform buffer and its bind group.
pub fn build_camera_uniform(device: &wgpu::Device, layouts: &Layouts) -> CameraUniform {
    use wgpu::util::DeviceExt;
    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("camera"),
        contents: bytemuck::cast_slice(&[[0.0f32; 4]; 4]),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("camera_bg"),
        layout: &layouts.camera_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buf.as_entire_binding(),
        }],
    });
    CameraUniform { buf, bind_group }
}

/// Create the zone, billboard, and character render pipelines.
pub fn build_pipelines(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    layouts: &Layouts,
) -> Pipelines {
    let vbl = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
    };

    let depth = wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: true,
        depth_compare: wgpu::CompareFunction::Less,
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState::default(),
    };

    // ── Zone pipeline ──────────────────────────────────────────────────────
    let zone_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("zone"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/zone.wgsl").into()),
    });
    let zone_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("zone_layout"),
        bind_group_layouts: &[&layouts.camera_bgl, &layouts.texture_bgl],
        push_constant_ranges: &[],
    });
    let zone = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("zone"),
        layout: Some(&zone_layout),
        vertex: wgpu::VertexState {
            module: &zone_shader, entry_point: "vs_main",
            buffers: std::slice::from_ref(&vbl), compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &zone_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,  // EQ zones are viewed from inside
            ..Default::default()
        },
        depth_stencil: Some(depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Billboard pipeline ─────────────────────────────────────────────────
    let bb_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("billboard"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/billboard.wgsl").into()),
    });
    let bb_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("billboard_layout"),
        bind_group_layouts: &[&layouts.camera_bgl],  // no texture slot — color in normal channel
        push_constant_ranges: &[],
    });
    let billboard = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("billboard"),
        layout: Some(&bb_layout),
        vertex: wgpu::VertexState {
            module: &bb_shader, entry_point: "vs_main",
            buffers: std::slice::from_ref(&vbl), compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &bb_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Character pipeline ─────────────────────────────────────────────────
    let char_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("character"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/character.wgsl").into()),
    });
    let char_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("character_layout"),
        bind_group_layouts: &[&layouts.camera_bgl, &layouts.texture_bgl, &layouts.entity_bgl],
        push_constant_ranges: &[],
    });
    let character = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("character"),
        layout: Some(&char_layout),
        vertex: wgpu::VertexState {
            module: &char_shader, entry_point: "vs_main",
            buffers: &[vbl], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &char_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Skinned character pipeline ─────────────────────────────────────────
    use crate::gpu::SkinnedVertex;
    let skinned_vbl = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<SkinnedVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x3,  // position
            1 => Float32x3,  // normal
            2 => Float32x2,  // uv
            3 => Uint32x4,   // joint_indices
            4 => Float32x4,  // joint_weights
        ],
    };
    let skinned_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("character_skinned"),
        source: wgpu::ShaderSource::Wgsl(
            include_str!("shaders/character_skinned.wgsl").into()
        ),
    });
    let skinned_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("skinned_layout"),
        bind_group_layouts: &[
            &layouts.camera_bgl,
            &layouts.texture_bgl,
            &layouts.entity_bgl,
            &layouts.joints_bgl,
        ],
        push_constant_ranges: &[],
    });
    let skinned = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("skinned"),
        layout: Some(&skinned_layout),
        vertex: wgpu::VertexState {
            module: &skinned_shader, entry_point: "vs_main",
            buffers: &[skinned_vbl], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &skinned_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: Some(depth),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Sky background pipeline ────────────────────────────────────────────
    // No vertex buffer — geometry is generated from vertex_index in the shader.
    // No depth test or write — sky is the background layer rendered first.
    let sky_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("sky"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/sky.wgsl").into()),
    });
    let sky_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("sky_layout"),
        bind_group_layouts: &[],
        push_constant_ranges: &[],
    });
    let sky = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("sky"),
        layout: Some(&sky_layout),
        vertex: wgpu::VertexState {
            module: &sky_shader, entry_point: "vs_main",
            buffers: &[], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &sky_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::REPLACE),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            cull_mode: None,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    Pipelines { sky, zone, billboard, character, skinned }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layouts_has_three_bgls() {
        fn _check(l: &Layouts) {
            let _: &wgpu::BindGroupLayout = &l.camera_bgl;
            let _: &wgpu::BindGroupLayout = &l.texture_bgl;
            let _: &wgpu::BindGroupLayout = &l.entity_bgl;
        }
    }

    #[test]
    fn camera_uniform_has_buf_and_bind_group() {
        fn _check(c: &CameraUniform) {
            let _: &wgpu::Buffer      = &c.buf;
            let _: &wgpu::BindGroup   = &c.bind_group;
        }
    }

    #[test]
    fn pipelines_has_all_pipelines() {
        fn _check(p: &Pipelines) {
            let _: &wgpu::RenderPipeline = &p.sky;
            let _: &wgpu::RenderPipeline = &p.zone;
            let _: &wgpu::RenderPipeline = &p.billboard;
            let _: &wgpu::RenderPipeline = &p.character;
        }
    }

    #[test]
    fn build_layouts_has_correct_signature() {
        let _: fn(&wgpu::Device) -> Layouts = build_layouts;
    }

    #[test]
    fn build_camera_uniform_has_correct_signature() {
        let _: fn(&wgpu::Device, &Layouts) -> CameraUniform = build_camera_uniform;
    }

    #[test]
    fn build_pipelines_has_correct_signature() {
        let _: fn(&wgpu::Device, wgpu::TextureFormat, &Layouts) -> Pipelines = build_pipelines;
    }

    #[test]
    fn layouts_has_joints_bgl() {
        fn _check(l: &Layouts) {
            let _: &wgpu::BindGroupLayout = &l.joints_bgl;
        }
    }

    #[test]
    fn pipelines_has_skinned() {
        fn _check(p: &Pipelines) {
            let _: &wgpu::RenderPipeline = &p.skinned;
        }
    }

    #[test]
    fn build_skinned_pipeline_has_correct_signature() {
        let _: fn(&wgpu::Device, wgpu::TextureFormat, &Layouts) -> Pipelines = build_pipelines;
    }
}
