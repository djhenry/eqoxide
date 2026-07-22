//! wgpu bind-group layouts (camera, texture, entity, joints), the camera uniform, and the render
//! pipelines + shaders for the zone, skinned-character, and billboard passes.

use crate::gpu::Vertex;

// Raw WGSL source for the character + skin-probe shaders, exposed so the `render_model` dev bin (in
// the app crate) can build its own probe pipelines from the SAME shader text the renderer uses,
// instead of reaching through the filesystem into this crate's private `shaders/` dir now that the
// shaders live here (#544 Step 2n). `CHARACTER_WGSL` is also the source `character_pipeline` binds
// below; `SKIN_PROBE_WGSL` is used only by the dev bin's GPU-skinning readback probe.
pub const CHARACTER_WGSL:  &str = include_str!("shaders/character.wgsl");
pub const SKIN_PROBE_WGSL: &str = include_str!("shaders/skin_probe.wgsl");

pub struct Layouts {
    pub camera_bgl:  wgpu::BindGroupLayout,
    pub texture_bgl: wgpu::BindGroupLayout,
    pub entity_bgl:  wgpu::BindGroupLayout,
    pub joints_bgl:  wgpu::BindGroupLayout,
    /// Time-of-day sky-gradient uniform (eqoxide#561): a single fragment-visible uniform buffer.
    pub sky_bgl:     wgpu::BindGroupLayout,
    /// Sun shadow map: the light view-proj uniform bound as a VERTEX uniform in the shadow DEPTH
    /// pass (group 0). Separate from `shadow_sample_bgl` (which the lit shaders read) so each side
    /// declares only what it needs. (#518)
    pub shadow_light_bgl: wgpu::BindGroupLayout,
    /// What the lit zone shaders read to receive shadows (group 2 on the zone pipelines): the light
    /// view-proj uniform + the shadow depth texture + a comparison sampler. (#518)
    pub shadow_sample_bgl: wgpu::BindGroupLayout,
    /// Weather-particle uniform (eqoxide#542): per-frame camera basis + fall params, visible to
    /// both stages (vertex animates the field, fragment shades rain vs snow).
    pub weather_bgl: wgpu::BindGroupLayout,
}

pub struct CameraUniform {
    pub buf:        wgpu::Buffer,
    pub bind_group: wgpu::BindGroup,
}

/// The sky-gradient uniform (eqoxide#561): zenith/horizon colors, rewritten each frame from the
/// time-of-day clock. Same shape as `CameraUniform` — a buffer plus its bind group.
pub struct SkyUniform {
    pub buf:        wgpu::Buffer,
    pub bind_group: wgpu::BindGroup,
}

/// GPU resources for the weather-particle field (eqoxide#542): a per-frame uniform (camera basis +
/// fall params), a static instance buffer of per-particle base positions, and the shared quad
/// vertex buffer. The particle count actually drawn is chosen per frame from the server weather
/// (see `eqoxide_core::weather::particle_plan`); the buffers are sized once for `MAX_PARTICLES`.
pub struct WeatherResources {
    pub uniform_buf:  wgpu::Buffer,
    pub bind_group:   wgpu::BindGroup,
    /// `MAX_PARTICLES` instances, each a vec4 (base xyz in [0,1) + phase). Generated once,
    /// deterministically — the shader recycles them around the moving camera every frame.
    pub instance_buf: wgpu::Buffer,
    /// 6 vertices (two triangles) of a unit quad, corners in [-1,1]^2, billboarded in the shader.
    pub quad_buf:     wgpu::Buffer,
}

pub struct Pipelines {
    pub sky:       wgpu::RenderPipeline,
    pub zone:      wgpu::RenderPipeline,
    pub zone_instanced: wgpu::RenderPipeline,
    /// Transparent zone variants drawn after the opaque pass (depth-write off):
    /// `*_blend` = src-alpha blend, `*_additive` = additive fire/glow.
    pub zone_blend: wgpu::RenderPipeline,
    pub zone_additive: wgpu::RenderPipeline,
    pub zone_instanced_blend: wgpu::RenderPipeline,
    pub zone_instanced_additive: wgpu::RenderPipeline,
    pub billboard: wgpu::RenderPipeline,
    pub character: wgpu::RenderPipeline,
    pub skinned:   wgpu::RenderPipeline,
    /// Second-pass variant of `skinned` for the cloth/armor overlay layer: same shader, but
    /// depth_compare = LessEqual and depth_write = false so the alpha-blended overlay composites
    /// on top of the already-drawn opaque skin base at the same depth (Luclin two-layer body art).
    pub skinned_overlay: wgpu::RenderPipeline,
    /// Sun shadow-map DEPTH pipelines (#518) — render casters from the light's POV into the shadow
    /// map (depth-only, no fragment/color target). One per geometry kind, mirroring the color
    /// passes: `shadow_static` (static mesh), `shadow_skinned` (skinned), `shadow_instanced`
    /// (placed objects).
    pub shadow_static:    wgpu::RenderPipeline,
    pub shadow_skinned:   wgpu::RenderPipeline,
    pub shadow_instanced: wgpu::RenderPipeline,
    /// Weather precipitation particles (eqoxide#542): instanced billboard quads, alpha-blended,
    /// depth-tested against the scene but depth-write off. Drawn only when weather is active.
    pub weather: wgpu::RenderPipeline,
    /// Nav diagnostics overlay (#608): world-space colored LINE LIST, alpha-blended, depth-tested
    /// against the scene (LessEqual) with depth-write off — so the overlay is correctly OCCLUDED by
    /// geometry (the whole point of replacing the screen-space egui painter) without disturbing the
    /// depth buffer. Drawn only when a `SceneState::nav_debug` snapshot is present (F11).
    pub nav_debug: wgpu::RenderPipeline,
}

/// Create the three bind group layouts used across all pipelines.
pub fn build_layouts(device: &wgpu::Device) -> Layouts {
    let camera_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("camera_bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            // FRAGMENT added alongside VERTEX (eqoxide#517): the fog fields riding along on this
            // uniform (camera_pos/fog_color/fog_params) are read in every fragment shader's
            // apply_fog(), not just the vertex stage's view_proj transform.
            visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
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

    let sky_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("sky_bgl"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::FRAGMENT,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });

    // Sun shadow map (#518). The depth pass binds only the light view-proj (vertex uniform).
    let shadow_light_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shadow_light_bgl"),
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

    let weather_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("weather_bgl"),
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

    // What the lit shaders sample: light view-proj (fragment uniform) + depth texture + comparison
    // sampler. Depth32Float is sampled as a non-filtering depth texture with a comparison sampler.
    let shadow_sample_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shadow_sample_bgl"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Comparison),
                count: None,
            },
        ],
    });

    Layouts { camera_bgl, texture_bgl, entity_bgl, joints_bgl, sky_bgl, shadow_light_bgl, shadow_sample_bgl, weather_bgl }
}

/// Create the weather-particle GPU resources (eqoxide#542): the per-frame uniform + bind group, the
/// static per-particle instance buffer (deterministically generated base positions), and the shared
/// billboard quad. The particle buffer is sized once for `weather::MAX_PARTICLES`; the renderer
/// draws a subset each frame based on the server weather intensity.
pub fn build_weather(device: &wgpu::Device, layouts: &Layouts) -> WeatherResources {
    use wgpu::util::DeviceExt;

    // Per-frame uniform, zero-initialized (the first active frame overwrites it; when weather is
    // clear the pass is skipped, so stale contents are never drawn).
    let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("weather_uniform"),
        size: std::mem::size_of::<crate::gpu::WeatherUniformData>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("weather_bg"),
        layout: &layouts.weather_bgl,
        entries: &[wgpu::BindGroupEntry { binding: 0, resource: uniform_buf.as_entire_binding() }],
    });

    // Instance data: MAX_PARTICLES base positions in the unit cube [0,1)^3 plus a per-particle
    // phase, from a small deterministic PRNG (xorshift32) so the field is identical every run and
    // needs no RNG crate. The shader maps these into the camera-centered box each frame.
    let n = eqoxide_core::weather::MAX_PARTICLES as usize;
    let mut instances: Vec<[f32; 4]> = Vec::with_capacity(n);
    let mut state: u32 = 0x9E3779B9; // fixed seed → deterministic field
    let mut next = || {
        // xorshift32
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        (state as f32) / (u32::MAX as f32)
    };
    for _ in 0..n {
        instances.push([next(), next(), next(), next()]);
    }
    let instance_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("weather_instances"),
        contents: bytemuck::cast_slice(&instances),
        usage: wgpu::BufferUsages::VERTEX,
    });

    // Static billboard quad: two triangles, corners in [-1,1]^2.
    let quad: [[f32; 2]; 6] = [
        [-1.0, -1.0], [1.0, -1.0], [1.0, 1.0],
        [-1.0, -1.0], [1.0, 1.0], [-1.0, 1.0],
    ];
    let quad_buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("weather_quad"),
        contents: bytemuck::cast_slice(&quad),
        usage: wgpu::BufferUsages::VERTEX,
    });

    WeatherResources { uniform_buf, bind_group, instance_buf, quad_buf }
}

/// Create the sky-gradient uniform buffer + bind group (eqoxide#561). Initialized to the daytime
/// default so the very first frame (before any `write_buffer`) still renders a sane sky.
pub fn build_sky_uniform(device: &wgpu::Device, layouts: &Layouts) -> SkyUniform {
    use wgpu::util::DeviceExt;
    let day = eqoxide_core::sky::sky_colors(eqoxide_core::sky::DEFAULT_HOUR);
    let init = crate::gpu::SkyUniformData {
        zenith:  [day.zenith[0], day.zenith[1], day.zenith[2], 0.0],
        horizon: [day.horizon[0], day.horizon[1], day.horizon[2], 0.0],
    };
    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("sky"),
        contents: bytemuck::bytes_of(&init),
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
    });
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("sky_bg"),
        layout: &layouts.sky_bgl,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: buf.as_entire_binding(),
        }],
    });
    SkyUniform { buf, bind_group }
}

/// Create the camera uniform buffer and its bind group. Sized for `gpu::CameraUniformData`
/// (view_proj + camera_pos + fog_color + fog_params, eqoxide#517) — the bind group layout itself
/// (`camera_bgl`) didn't need to change since it already covers "the whole buffer" at binding 0.
pub fn build_camera_uniform(device: &wgpu::Device, layouts: &Layouts) -> CameraUniform {
    use wgpu::util::DeviceExt;
    let init = crate::gpu::CameraUniformData {
        view_proj:  [[0.0; 4]; 4],
        camera_pos: [0.0; 4],
        fog_color:  [0.0; 4],
        fog_params: [0.0; 4],
    };
    let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
        label: Some("camera"),
        contents: bytemuck::bytes_of(&init),
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
    // group 2 = shadow sampling (#518): terrain + placed objects receive sun shadows.
    let zone_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("zone_layout"),
        bind_group_layouts: &[&layouts.camera_bgl, &layouts.texture_bgl, &layouts.shadow_sample_bgl],
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

    // ── Zone instanced pipeline ────────────────────────────────────────────
    // Same bind groups + targets as `zone`, but with a second (Instance step-mode)
    // vertex buffer carrying a column-major 4×4 matrix as four Float32x4 attributes.
    let instance_vbl = wgpu::VertexBufferLayout {
        array_stride: 64,
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![
            3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4
        ],
    };
    let zone_inst_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("zone_instanced"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/zone_instanced.wgsl").into()),
    });
    let zone_instanced = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("zone_instanced"),
        layout: Some(&zone_layout),
        vertex: wgpu::VertexState {
            module: &zone_inst_shader, entry_point: "vs_main",
            buffers: &[vbl.clone(), instance_vbl.clone()], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &zone_inst_shader, entry_point: "fs_main",
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

    // ── Transparent zone pipelines (blend + additive, static + instanced) ──────
    // Drawn after the opaque/masked pass with depth-write OFF (so they don't occlude
    // each other or geometry behind them). They use the zone shaders' `fs_blend` entry
    // (no alpha-test discard). Blend opacity is baked into the texture alpha by the
    // asset server; additive fire/glow uses pure src+dst add (black texels add nothing).
    let transparent_depth = wgpu::DepthStencilState {
        depth_write_enabled: false,
        depth_compare: wgpu::CompareFunction::LessEqual,
        ..depth.clone()
    };
    let additive_blend = wgpu::BlendState {
        color: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
        alpha: wgpu::BlendComponent {
            src_factor: wgpu::BlendFactor::One,
            dst_factor: wgpu::BlendFactor::One,
            operation: wgpu::BlendOperation::Add,
        },
    };

    let zone_blend = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("zone_blend"),
        layout: Some(&zone_layout),
        vertex: wgpu::VertexState {
            module: &zone_shader, entry_point: "vs_main",
            buffers: std::slice::from_ref(&vbl), compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &zone_shader, entry_point: "fs_blend",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(transparent_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    let zone_additive = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("zone_additive"),
        layout: Some(&zone_layout),
        vertex: wgpu::VertexState {
            module: &zone_shader, entry_point: "vs_main",
            buffers: std::slice::from_ref(&vbl), compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            // Additive glow (lava/fire/torches) must attenuate toward zero as fog deepens, not
            // mix toward fog_color — the fixed-function blend below is a pure One/One add with
            // no destination term to mix against. `fs_blend_additive` uses `apply_fog_additive`
            // for that; `fs_blend` (mix-to-fog_color) is only correct under ALPHA_BLENDING
            // (review defect on #523 — see zone.wgsl's apply_fog_additive doc comment).
            module: &zone_shader, entry_point: "fs_blend_additive",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(additive_blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(transparent_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    let zone_instanced_blend = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("zone_instanced_blend"),
        layout: Some(&zone_layout),
        vertex: wgpu::VertexState {
            module: &zone_inst_shader, entry_point: "vs_main",
            buffers: &[vbl.clone(), instance_vbl.clone()], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &zone_inst_shader, entry_point: "fs_blend",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(transparent_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    let zone_instanced_additive = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("zone_instanced_additive"),
        layout: Some(&zone_layout),
        vertex: wgpu::VertexState {
            module: &zone_inst_shader, entry_point: "vs_main",
            buffers: &[vbl.clone(), instance_vbl.clone()], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            // See zone_additive above: additive glow must attenuate toward zero under the
            // fixed-function One/One add, so this binds `fs_blend_additive` (review defect on
            // #523), not the mix-to-fog_color `fs_blend` used by zone_instanced_blend.
            module: &zone_inst_shader, entry_point: "fs_blend_additive",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(additive_blend),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(transparent_depth.clone()),
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
            buffers: std::slice::from_ref(&skinned_vbl), compilation_options: Default::default(),
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
        depth_stencil: Some(depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Skinned overlay pipeline (Luclin two-layer body: cloth/armor over skin) ──
    // Identical to `skinned` except the depth state: LessEqual + no depth write, so the
    // alpha-blended overlay draws on top of the opaque skin base already laid down at the
    // same surface depth. Where the overlay's texel alpha is 0 (e.g. an exposed midriff in
    // elfch0003), alpha blending leaves the skin showing through.
    let skinned_overlay = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("skinned_overlay"),
        layout: Some(&skinned_layout),
        vertex: wgpu::VertexState {
            module: &skinned_shader, entry_point: "vs_main",
            buffers: std::slice::from_ref(&skinned_vbl), compilation_options: Default::default(),
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
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: false,
            depth_compare: wgpu::CompareFunction::LessEqual,
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
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
        bind_group_layouts: &[&layouts.sky_bgl],
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

    // ── Sun shadow-map depth pipelines (#518) ──────────────────────────────
    // Depth-only: no fragment stage, no color target. A slope-scaled depth bias in hardware pushes
    // caster depths away from receivers to fight shadow acne (paired with the small shader-side
    // epsilon). Casters draw with cull_mode:None to match the color passes (EQ meshes aren't
    // consistently wound). Vertex buffer layouts reuse the color passes' formats.
    let shadow_depth = wgpu::DepthStencilState {
        format: wgpu::TextureFormat::Depth32Float,
        depth_write_enabled: true,
        depth_compare: wgpu::CompareFunction::Less,
        stencil: wgpu::StencilState::default(),
        bias: wgpu::DepthBiasState { constant: 2, slope_scale: 2.0, clamp: 0.0 },
    };
    let shadow_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("shadow"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/shadow.wgsl").into()),
    });

    // Reuse the color passes' vertex layouts so casters feed the same buffers.
    let shadow_vbl = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<Vertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x3, 2 => Float32x2],
    };
    let shadow_skinned_vbl = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<SkinnedVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![
            0 => Float32x3, 1 => Float32x3, 2 => Float32x2, 3 => Uint32x4, 4 => Float32x4],
    };

    // Static caster: group 0 = light vp, group 1 = model uniform (entity_bgl).
    let shadow_static_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shadow_static_layout"),
        bind_group_layouts: &[&layouts.shadow_light_bgl, &layouts.entity_bgl],
        push_constant_ranges: &[],
    });
    let shadow_static = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("shadow_static"),
        layout: Some(&shadow_static_layout),
        vertex: wgpu::VertexState {
            module: &shadow_shader, entry_point: "vs_static",
            buffers: std::slice::from_ref(&shadow_vbl), compilation_options: Default::default(),
        },
        fragment: None,
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(shadow_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // Skinned caster: adds group 2 = joint palette.
    let shadow_skinned_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shadow_skinned_layout"),
        bind_group_layouts: &[&layouts.shadow_light_bgl, &layouts.entity_bgl, &layouts.joints_bgl],
        push_constant_ranges: &[],
    });
    let shadow_skinned = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("shadow_skinned"),
        layout: Some(&shadow_skinned_layout),
        vertex: wgpu::VertexState {
            module: &shadow_shader, entry_point: "vs_skinned",
            buffers: std::slice::from_ref(&shadow_skinned_vbl), compilation_options: Default::default(),
        },
        fragment: None,
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(shadow_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // Instanced caster: group 0 = light vp only; per-instance matrix via the instance vertex buffer.
    let shadow_instanced_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shadow_instanced_layout"),
        bind_group_layouts: &[&layouts.shadow_light_bgl],
        push_constant_ranges: &[],
    });
    let shadow_instanced = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("shadow_instanced"),
        layout: Some(&shadow_instanced_layout),
        vertex: wgpu::VertexState {
            module: &shadow_shader, entry_point: "vs_instanced",
            buffers: &[shadow_vbl.clone(), instance_vbl.clone()], compilation_options: Default::default(),
        },
        fragment: None,
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(shadow_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Weather particle pipeline (eqoxide#542) ────────────────────────────
    // Instanced billboard quads: group 0 = camera (view_proj + camera_pos), group 1 = weather
    // params. vbuf 0 = the static quad corner (vec2, per-vertex); vbuf 1 = per-particle instance
    // (vec4 base+phase, per-instance). Alpha-blended and depth-tested against the scene but with
    // depth-write OFF (reuses `transparent_depth`), so precipitation is occluded by geometry in
    // front of it without polluting depth for later passes.
    let weather_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("weather"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/weather.wgsl").into()),
    });
    let weather_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("weather_layout"),
        bind_group_layouts: &[&layouts.camera_bgl, &layouts.weather_bgl],
        push_constant_ranges: &[],
    });
    let weather_quad_vbl = wgpu::VertexBufferLayout {
        array_stride: 8, // vec2<f32>
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x2],
    };
    let weather_inst_vbl = wgpu::VertexBufferLayout {
        array_stride: 16, // vec4<f32> base+phase
        step_mode: wgpu::VertexStepMode::Instance,
        attributes: &wgpu::vertex_attr_array![1 => Float32x4],
    };
    let weather = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("weather"),
        layout: Some(&weather_layout),
        vertex: wgpu::VertexState {
            module: &weather_shader, entry_point: "vs_main",
            buffers: &[weather_quad_vbl, weather_inst_vbl], compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &weather_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(transparent_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    // ── Nav diagnostics overlay pipeline (#608) ────────────────────────────
    // World-space colored lines. Depth: `transparent_depth` (LessEqual test, write OFF) — the
    // overlay is occluded by walls/floors exactly like any world geometry, but never occludes
    // anything itself. Colors pass straight through from the vertex data (see nav_overlay.rs).
    let nav_debug_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("nav_debug"),
        source: wgpu::ShaderSource::Wgsl(include_str!("shaders/nav_debug.wgsl").into()),
    });
    let nav_debug_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("nav_debug_layout"),
        bind_group_layouts: &[&layouts.camera_bgl],
        push_constant_ranges: &[],
    });
    let nav_debug_vbl = wgpu::VertexBufferLayout {
        array_stride: std::mem::size_of::<crate::nav_overlay::OverlayVertex>() as u64,
        step_mode: wgpu::VertexStepMode::Vertex,
        attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x4],
    };
    let nav_debug = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("nav_debug"),
        layout: Some(&nav_debug_layout),
        vertex: wgpu::VertexState {
            module: &nav_debug_shader, entry_point: "vs_main",
            buffers: std::slice::from_ref(&nav_debug_vbl), compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module: &nav_debug_shader, entry_point: "fs_main",
            targets: &[Some(wgpu::ColorTargetState {
                format, blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::LineList, cull_mode: None, ..Default::default()
        },
        depth_stencil: Some(transparent_depth.clone()),
        multisample: wgpu::MultisampleState::default(),
        multiview: None, cache: None,
    });

    Pipelines {
        sky, zone, zone_instanced,
        zone_blend, zone_additive, zone_instanced_blend, zone_instanced_additive,
        billboard, character, skinned, skinned_overlay, weather,
        shadow_static, shadow_skinned, shadow_instanced,
        nav_debug,
    }
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
            let _: &wgpu::RenderPipeline = &p.zone_instanced;
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

    #[test]
    fn layouts_has_shadow_bgls() {
        // The two shadow bind-group layouts (#518) must exist: one for the depth pass (vertex-side
        // light matrix), one for the lit shaders (light matrix + depth texture + comparison sampler).
        fn _check(l: &Layouts) {
            let _: &wgpu::BindGroupLayout = &l.shadow_light_bgl;
            let _: &wgpu::BindGroupLayout = &l.shadow_sample_bgl;
        }
    }

    #[test]
    fn pipelines_has_shadow_pipelines() {
        // All three shadow-map depth pipelines (#518) must be present on the Pipelines struct.
        fn _check(p: &Pipelines) {
            let _: &wgpu::RenderPipeline = &p.shadow_static;
            let _: &wgpu::RenderPipeline = &p.shadow_skinned;
            let _: &wgpu::RenderPipeline = &p.shadow_instanced;
        }
    }
}
