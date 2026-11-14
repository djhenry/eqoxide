//! Opt-in bounded HDR surface compositing; historical borrowed-pass drawing stays separate.
use super::{
    product, scope_result, GpuStaticVisual, PreparedStaticVisual, ReferenceLighting, RenderOutput,
    UploadLimits, VisualGpuStats,
};
use anyhow::{ensure, Context, Result};
use eqoxide_assets::static_visual::{Bounds, StaticRenderProfile};
use glam::Mat4;
use std::panic::{catch_unwind, AssertUnwindSafe};
use wgpu::util::DeviceExt;

const POSTPROCESS_BYTES: usize = 16;
const HDR_MAX: f32 = 65504.;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrFrameLimits {
    pub max_dimension: u32,
    /// Retained HDR color and depth payload; not peak driver or process allocation.
    pub max_frame_bytes: usize,
}
impl Default for HdrFrameLimits {
    fn default() -> Self {
        Self {
            max_dimension: 16384,
            max_frame_bytes: 128 << 20,
        }
    }
}
/// Renderer-owned payloads. Caller display/capture targets and driver overhead are excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrGpuStats {
    pub scene: VisualGpuStats,
    pub postprocess_bytes: usize,
    pub frame_target_bytes: usize,
    pub total_gpu_bytes: usize,
    pub width: u32,
    pub height: u32,
}
struct Targets {
    _color: wgpu::Texture,
    _depth: wgpu::Texture,
    color: wgpu::TextureView,
    depth: wgpu::TextureView,
    display: wgpu::BindGroup,
    width: u32,
    height: u32,
    bytes: usize,
}
/// Shared scalar-PBR HDR inspector, selected only by its explicit artifact capability.
///
/// Radiance uses the documented finite half-float range. This is an opaque display
/// compositor, not transparent-canvas export or a certificate of GPU completion.
pub struct GpuHdrStaticVisual {
    scene: GpuStaticVisual,
    display_pipeline: wgpu::RenderPipeline,
    display_layout: wgpu::BindGroupLayout,
    display_uniform: wgpu::Buffer,
    targets: Option<Targets>,
    targets_ready: bool,
    max_gpu_bytes: usize,
    frame_limits: HdrFrameLimits,
}
fn gpu_resource<T>(
    device: &wgpu::Device,
    label: &str,
    build: impl FnOnce() -> Result<T>,
) -> Result<T> {
    for filter in [
        wgpu::ErrorFilter::OutOfMemory,
        wgpu::ErrorFilter::Internal,
        wgpu::ErrorFilter::Validation,
    ] {
        device.push_error_scope(filter);
    }
    let result = catch_unwind(AssertUnwindSafe(build))
        .map_err(|_| anyhow::anyhow!("{label} dependency panicked"));
    let mut errors = Vec::new();
    for _ in 0..3 {
        if let Some(error) = scope_result(device) {
            errors.push(error.to_string());
        }
    }
    ensure!(errors.is_empty(), "{label} failed: {}", errors.join("; "));
    result?
}
impl GpuHdrStaticVisual {
    pub fn upload(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        prepared: PreparedStaticVisual,
        output_format: wgpu::TextureFormat,
        lighting: ReferenceLighting,
        upload_limits: &UploadLimits,
        frame_limits: &HdrFrameLimits,
    ) -> Result<Self> {
        ensure!(
            prepared.profile() == StaticRenderProfile::PbrHdrBlendReferenceLight,
            "HDR encoder requires the declared HDR blend profile"
        );
        ensure!(
            matches!(
                output_format,
                wgpu::TextureFormat::Rgba8UnormSrgb | wgpu::TextureFormat::Bgra8UnormSrgb
            ),
            "HDR display requires RGBA8/BGRA8 sRGB output"
        );
        let stats = prepared.stats();
        ensure!(
            stats.draw_commands <= upload_limits.max_draw_commands,
            "HDR scene exceeds explicit draw-command limit"
        );
        ensure!(
            stats.scene_triangle_instances <= upload_limits.max_triangle_instances,
            "HDR scene exceeds explicit triangle-instance limit"
        );
        ensure!(
            16 <= upload_limits.max_texture_bytes,
            "white fallback exceeds HDR explicit texture payload limit"
        );
        for image in &prepared.images {
            let bytes = product(image.pixels.len(), std::mem::size_of::<f32>())?;
            ensure!(
                bytes <= upload_limits.max_texture_bytes,
                "HDR image exceeds explicit texture payload limit"
            );
        }
        let base = prepared
            .stats()
            .gpu_bytes
            .checked_add(POSTPROCESS_BYTES)
            .context("HDR postprocess payload overflow")?;
        ensure!(
            base <= upload_limits.max_gpu_bytes,
            "HDR postprocess exceeds aggregate GPU payload limit"
        );
        let features =
            wgpu::TextureFormat::Rgba16Float.guaranteed_format_features(device.features());
        ensure!(
            features.allowed_usages.contains(
                wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING
            ) && features
                .flags
                .contains(wgpu::TextureFormatFeatureFlags::BLENDABLE),
            "device cannot represent baseline RGBA16F HDR blending"
        );
        let scene = GpuStaticVisual::upload_for_output(
            device,
            queue,
            prepared,
            wgpu::TextureFormat::Rgba16Float,
            lighting,
            RenderOutput::Hdr,
        )?;
        let (display_layout, display_uniform, display_pipeline) =
            gpu_resource(device, "HDR display upload", || {
                let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("HDR display inputs"),
                    entries: &[
                        wgpu::BindGroupLayoutEntry {
                            binding: 0,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Texture {
                                sample_type: wgpu::TextureSampleType::Float { filterable: false },
                                view_dimension: wgpu::TextureViewDimension::D2,
                                multisampled: false,
                            },
                            count: None,
                        },
                        wgpu::BindGroupLayoutEntry {
                            binding: 1,
                            visibility: wgpu::ShaderStages::FRAGMENT,
                            ty: wgpu::BindingType::Buffer {
                                ty: wgpu::BufferBindingType::Uniform,
                                has_dynamic_offset: false,
                                min_binding_size: wgpu::BufferSize::new(POSTPROCESS_BYTES as u64),
                            },
                            count: None,
                        },
                    ],
                });
                let uniform = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                    label: Some("HDR display exposure"),
                    contents: bytemuck::cast_slice(&[lighting.exposure, 0., 0., 0.]),
                    usage: wgpu::BufferUsages::UNIFORM,
                });
                let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                    label: Some("HDR single display mapping"),
                    source: wgpu::ShaderSource::Wgsl(
                        include_str!("../shaders/static_visual_display.wgsl").into(),
                    ),
                });
                let pipeline_layout =
                    device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        label: Some("HDR display layout"),
                        bind_group_layouts: &[&layout],
                        push_constant_ranges: &[],
                    });
                let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                    label: Some("HDR display"),
                    layout: Some(&pipeline_layout),
                    vertex: wgpu::VertexState {
                        module: &shader,
                        entry_point: "vs_display",
                        buffers: &[],
                        compilation_options: Default::default(),
                    },
                    fragment: Some(wgpu::FragmentState {
                        module: &shader,
                        entry_point: "fs_display",
                        targets: &[Some(wgpu::ColorTargetState {
                            format: output_format,
                            blend: None,
                            write_mask: wgpu::ColorWrites::ALL,
                        })],
                        compilation_options: Default::default(),
                    }),
                    primitive: Default::default(),
                    depth_stencil: None,
                    multisample: Default::default(),
                    multiview: None,
                    cache: None,
                });
                Ok((layout, uniform, pipeline))
            })?;
        Ok(Self {
            scene,
            display_pipeline,
            display_layout,
            display_uniform,
            targets: None,
            targets_ready: false,
            max_gpu_bytes: upload_limits.max_gpu_bytes,
            frame_limits: *frame_limits,
        })
    }
    /// Reuse an accepted equal size. Failed attempts invalidate encoding until recovery.
    /// A candidate can coexist with old targets, so retained payload limits are not peak memory.
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<()> {
        self.targets_ready = false;
        ensure!(
            width > 0 && height > 0,
            "HDR frame dimensions must be positive"
        );
        let maximum = self
            .frame_limits
            .max_dimension
            .min(device.limits().max_texture_dimension_2d);
        ensure!(
            width <= maximum && height <= maximum,
            "HDR frame exceeds dimension limit"
        );
        let bytes = product(product(width as usize, height as usize)?, 12)
            .context("HDR frame payload overflow")?;
        ensure!(
            bytes <= self.frame_limits.max_frame_bytes,
            "HDR frame exceeds target payload limit"
        );
        let total = self
            .scene
            .stats()
            .gpu_bytes
            .checked_add(POSTPROCESS_BYTES)
            .and_then(|n| n.checked_add(bytes))
            .context("HDR aggregate payload overflow")?;
        ensure!(
            total <= self.max_gpu_bytes,
            "HDR frame exceeds aggregate GPU payload limit"
        );
        if self
            .targets
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height)
        {
            self.targets_ready = true;
            return Ok(());
        }
        let candidate = gpu_resource(device, "HDR target resize", || {
            let size = wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            };
            let color = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("HDR composed radiance"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING,
                view_formats: &[],
            });
            let depth = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("HDR solid depth"),
                size,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            });
            let color_view = color.create_view(&Default::default());
            let depth_view = depth.create_view(&Default::default());
            let display = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("HDR display target"),
                layout: &self.display_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(&color_view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: self.display_uniform.as_entire_binding(),
                    },
                ],
            });
            Ok(Targets {
                _color: color,
                _depth: depth,
                color: color_view,
                depth: depth_view,
                display,
                width,
                height,
                bytes,
            })
        })?;
        self.targets = Some(candidate);
        self.targets_ready = true;
        Ok(())
    }
    pub fn prepare_camera(
        &mut self,
        queue: &wgpu::Queue,
        view: Mat4,
        projection: Mat4,
    ) -> Result<()> {
        self.scene.prepare_camera(queue, view, projection)
    }
    /// Encode two full passes into a caller-owned matching output view, without submission.
    /// The caller must supply the upload format and accepted target extent.
    pub fn encode(
        &self,
        encoder: &mut wgpu::CommandEncoder,
        output: &wgpu::TextureView,
        clear_rgb: [f32; 3],
    ) -> Result<()> {
        ensure!(
            self.targets_ready,
            "HDR targets must be successfully resized before encoding"
        );
        ensure!(
            self.scene.camera_prepared,
            "HDR camera must be successfully prepared before encoding"
        );
        ensure!(
            clear_rgb
                .into_iter()
                .all(|v| v.is_finite() && (0.0..=HDR_MAX).contains(&v)),
            "HDR background must be finite within 0..=65504"
        );
        let targets = self
            .targets
            .as_ref()
            .context("HDR target state unavailable")?;
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("HDR static geometry and transparency"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &targets.color,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: f64::from(clear_rgb[0]),
                            g: f64::from(clear_rgb[1]),
                            b: f64::from(clear_rgb[2]),
                            a: 1.,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &targets.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.scene.draw(&mut pass)?;
        }
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("HDR final display"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: output,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.display_pipeline);
            pass.set_bind_group(0, &targets.display, &[]);
            pass.draw(0..3, 0..1);
        }
        Ok(())
    }
    pub fn bounds(&self) -> &Bounds {
        self.scene.bounds()
    }
    pub fn stats(&self) -> HdrGpuStats {
        let scene = self.scene.stats();
        let (frame_target_bytes, width, height) = self
            .targets
            .as_ref()
            .map_or((0, 0, 0), |t| (t.bytes, t.width, t.height));
        HdrGpuStats {
            scene,
            postprocess_bytes: POSTPROCESS_BYTES,
            frame_target_bytes,
            total_gpu_bytes: scene.gpu_bytes + POSTPROCESS_BYTES + frame_target_bytes,
            width,
            height,
        }
    }
}
