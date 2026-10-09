//! Separate, unlit GPU inspection of the validated common static visual profile.
//! This module does not activate a scene or establish gameplay/runtime readiness.
use anyhow::{ensure, Context, Result};
use bytemuck::{Pod, Zeroable};
use eqoxide_assets::static_visual::{decode_static_visual, Bounds, DecodeLimits, StaticAlphaMode, StaticVisual};
use glam::{DMat4, DVec3, Mat4};
use std::{future::Future, io::Cursor, panic::{catch_unwind, AssertUnwindSafe}, sync::Arc, task::{Poll, Wake, Waker}};
use wgpu::util::DeviceExt;

/// GPU buffer/texture payload limits, independent of total process memory.
#[derive(Debug, Clone)]
pub struct UploadLimits {
    pub max_gpu_bytes: usize,
    /// Maximum encoded draw commands, bounding primitive-instance blend sorting.
    pub max_draw_commands: usize,
    /// Individual float32 RGBA image payload, including the white fallback.
    pub max_texture_bytes: usize,
}
impl Default for UploadLimits {
    fn default() -> Self { Self { max_gpu_bytes: 512 << 20, max_texture_bytes: 64 << 20, max_draw_commands: 1_000_000 } }
}
/// Logical resources and allocated GPU payload. Texture aliases share image storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VisualGpuStats {
    pub mesh_count: usize,
    pub shared_vertex_buffers: usize,
    pub index_buffers: usize,
    pub color_buffers: usize,
    pub instance_records: usize,
    pub primitive_count: usize,
    pub image_count: usize,
    pub texture_count: usize,
    pub gpu_bytes: usize,
    pub draw_commands: usize,
}
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Vertex { position: [f32; 3], uv: [f32; 2] }
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct MaterialUniform { factor: [f32; 4], cutoff: f32, mode: u32, padding: [u32; 2] }
struct FloatImage { width: u32, height: u32, pixels: Vec<[f32; 4]> }
/// Validated scene: the only constructor calls the bounded common CPU decoder.
/// Private fields prevent uploading externally fabricated, unchecked scene data.
pub struct PreparedStaticVisual {
    scene: StaticVisual,
    images: Vec<FloatImage>,
    stats: VisualGpuStats,
    buffer_sizes: Vec<usize>,
}
fn product(n: usize, stride: usize) -> Result<usize> { n.checked_mul(stride).context("GPU payload multiplication overflow") }
fn add(total: &mut usize, bytes: usize) -> Result<()> { *total = total.checked_add(bytes).context("GPU payload addition overflow")?; Ok(()) }
impl PreparedStaticVisual {
    pub fn decode(bytes: &[u8], limits: &DecodeLimits, upload: &UploadLimits) -> Result<Self> {
        let scene = decode_static_visual(bytes, limits).context("static visual CPU decode")?;
        let mut stats = VisualGpuStats {
            mesh_count: scene.meshes.len(), shared_vertex_buffers: scene.meshes.len(),
            index_buffers: 0, color_buffers: 0, instance_records: scene.instances.len(),
            primitive_count: 0, image_count: scene.images.len(), texture_count: scene.textures.len(), gpu_bytes: 0, draw_commands: 0,
        };
        let mut instance_counts = vec![0usize; scene.meshes.len()];
        for instance in &scene.instances { instance_counts[instance.mesh_index] += 1; }
        let mut buffer_sizes = vec![64]; // view-projection uniform
        for (m, mesh) in scene.meshes.iter().enumerate() {
            buffer_sizes.push(product(mesh.positions.len(), 20)?);
            for primitive in &mesh.primitives {
                stats.primitive_count += 1;
                stats.index_buffers += 1;
                let n = if scene.materials[primitive.material_index].alpha_mode == StaticAlphaMode::Blend { instance_counts[m] } else { usize::from(instance_counts[m] != 0) };
                add(&mut stats.draw_commands, n)?;
                ensure!(stats.draw_commands <= upload.max_draw_commands, "static draw commands exceed upload limit");
                ensure!(u32::try_from(primitive.indices.len()).is_ok(), "primitive index count exceeds GPU draw range");
                buffer_sizes.push(product(primitive.indices.len(), 4)?);
                if primitive.colors.is_some() {
                    stats.color_buffers += 1;
                    buffer_sizes.push(product(mesh.positions.len(), 16)?);
                }
            }
            if mesh.primitives.iter().any(|p| p.colors.is_none()) {
                stats.color_buffers += 1;
                buffer_sizes.push(product(mesh.positions.len(), 16)?);
            }
        }
        for &n in &instance_counts {
            ensure!(u32::try_from(n).is_ok(), "instance count exceeds GPU draw range");
            // wgpu requires nonempty buffers even for unused meshes.
            buffer_sizes.push(product(n.max(1), 64)?);
        }
        for _ in &scene.materials { buffer_sizes.push(32); }
        for &n in &buffer_sizes { add(&mut stats.gpu_bytes, n)?; }
        ensure!(16 <= upload.max_texture_bytes, "white fallback exceeds texture payload limit");
        add(&mut stats.gpu_bytes, 16)?;
        for image in &scene.images {
            let n = product(product(image.width as usize, image.height as usize)?, 16)?;
            ensure!(n <= upload.max_texture_bytes, "float image exceeds texture payload limit");
            add(&mut stats.gpu_bytes, n)?;
        }
        ensure!(stats.gpu_bytes <= upload.max_gpu_bytes, "aggregate GPU payload exceeds upload limit");
        // Budget preflight precedes float image allocation. The bounded decoder has already
        // validated exact PNG bytes; enforce its decoder limits again while decoding pixels.
        let images = catch_unwind(AssertUnwindSafe(|| -> Result<Vec<FloatImage>> {
            scene.images.iter().map(|source| {
                let mut reader = image::ImageReader::with_format(Cursor::new(&source.png_bytes), image::ImageFormat::Png);
                let mut image_limits = image::Limits::default();
                image_limits.max_image_width = Some(limits.max_png_dimension);
                image_limits.max_image_height = Some(limits.max_png_dimension);
                image_limits.max_alloc = Some(limits.max_single_png_bytes as u64);
                reader.limits(image_limits);
                let decoded = reader.decode().context("bounded static PNG pixel decode")?.into_rgba32f();
                ensure!(decoded.width() == source.width && decoded.height() == source.height, "PNG dimensions changed after validation");
                let pixels = decoded.pixels().map(|p| [srgb_to_linear(p[0]), srgb_to_linear(p[1]), srgb_to_linear(p[2]), p[3]]).collect();
                Ok(FloatImage { width: source.width, height: source.height, pixels })
            }).collect()
        })).map_err(|_| anyhow::anyhow!("PNG dependency panicked during float preparation"))??;
        Ok(Self { scene, images, stats, buffer_sizes })
    }
    pub fn bounds(&self) -> &Bounds { &self.scene.bounds }
    pub fn stats(&self) -> VisualGpuStats { self.stats }
}
fn srgb_to_linear(v: f32) -> f32 { if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) } }
struct GpuPrimitive { indices: wgpu::Buffer, count: u32, colors: Option<wgpu::Buffer>, material: usize, center: DVec3 }
struct GpuMesh { vertices: wgpu::Buffer, white: Option<wgpu::Buffer>, instances: wgpu::Buffer, matrices: Vec<DMat4>, primitives: Vec<GpuPrimitive> }
struct GpuMaterial { group: wgpu::BindGroup, pipeline: usize, blend: bool }
#[derive(Clone, Copy)]
struct BlendDraw { mesh: usize, primitive: usize, instance: u32, depth: f64 }
/// Complete GPU resources for unlit inspection. Callers own the pass and submission.
/// The pass must use the upload color format, Depth32Float and sample count one.
pub struct GpuStaticVisual {
    bounds: Bounds, stats: VisualGpuStats, meshes: Vec<GpuMesh>, materials: Vec<GpuMaterial>,
    pipelines: Vec<wgpu::RenderPipeline>, camera: wgpu::Buffer, camera_group: wgpu::BindGroup,
    blend_draws: Vec<BlendDraw>, camera_prepared: bool,
}
struct ScopeWake(std::thread::Thread);
impl Wake for ScopeWake { fn wake(self: Arc<Self>) { self.0.unpark(); } }
fn scope_result(device: &wgpu::Device) -> Option<wgpu::Error> {
    let future = device.pop_error_scope();
    let mut future = std::pin::pin!(future);
    let waker = Waker::from(Arc::new(ScopeWake(std::thread::current())));
    let mut context = std::task::Context::from_waker(&waker);
    loop {
        device.poll(wgpu::Maintain::Wait);
        match future.as_mut().poll(&mut context) {
            Poll::Ready(error) => return error,
            Poll::Pending => std::thread::park_timeout(std::time::Duration::from_millis(1)),
        }
    }
}
impl GpuStaticVisual {
    /// Upload only after all payload and device-size checks succeed. Scoped GPU
    /// validation/allocation errors and dependency panics return an explicit error.
    pub fn upload(device: &wgpu::Device, queue: &wgpu::Queue, scene: PreparedStaticVisual, format: wgpu::TextureFormat) -> Result<Self> {
        ensure!(matches!(format, wgpu::TextureFormat::Rgba8Unorm | wgpu::TextureFormat::Rgba8UnormSrgb), "static inspection requires RGBA8 unorm or sRGB output");
        let limits = device.limits();
        ensure!(limits.max_vertex_buffers >= 3 && limits.max_vertex_attributes >= 7 && limits.max_vertex_buffer_array_stride >= 64,
            "device vertex limits cannot represent shared static instances");
        ensure!(limits.max_uniform_buffer_binding_size >= 64 && limits.max_bind_groups >= 2 && limits.max_sampled_textures_per_shader_stage >= 1,
            "device binding limits cannot represent static materials");
        for &n in &scene.buffer_sizes { ensure!(n as u64 <= limits.max_buffer_size, "static buffer exceeds device max_buffer_size"); }
        for image in &scene.images {
            ensure!(image.width <= limits.max_texture_dimension_2d && image.height <= limits.max_texture_dimension_2d,
                "static image exceeds device texture dimension limit");
        }
        for filter in [wgpu::ErrorFilter::OutOfMemory, wgpu::ErrorFilter::Internal, wgpu::ErrorFilter::Validation] { device.push_error_scope(filter); }
        let result = catch_unwind(AssertUnwindSafe(|| Self::upload_inner(device, queue, scene, format)))
            .map_err(|_| anyhow::anyhow!("static GPU dependency panicked during upload"));
        let mut errors = Vec::new();
        for _ in 0..3 { if let Some(error) = scope_result(device) { errors.push(error.to_string()); } }
        ensure!(errors.is_empty(), "static GPU upload failed: {}", errors.join("; "));
        result?
    }
    fn upload_inner(device: &wgpu::Device, queue: &wgpu::Queue, prepared: PreparedStaticVisual, format: wgpu::TextureFormat) -> Result<Self> {
        let PreparedStaticVisual { scene, images, stats, .. } = prepared;
        let camera_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("static visual camera layout"), entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0, visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(64) }, count: None,
            }],
        });
        let material_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("static visual material layout"), entries: &[
                wgpu::BindGroupLayoutEntry { binding: 0, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer { ty: wgpu::BufferBindingType::Uniform, has_dynamic_offset: false, min_binding_size: wgpu::BufferSize::new(32) }, count: None },
                wgpu::BindGroupLayoutEntry { binding: 1, visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture { sample_type: wgpu::TextureSampleType::Float { filterable: false }, view_dimension: wgpu::TextureViewDimension::D2, multisampled: false }, count: None },
            ],
        });
        let camera = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("static visual camera"), contents: bytemuck::cast_slice(&Mat4::IDENTITY.to_cols_array()), usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST });
        let camera_group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("static visual camera"), layout: &camera_layout, entries: &[wgpu::BindGroupEntry { binding: 0, resource: camera.as_entire_binding() }] });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor { label: Some("unlit static visual"), source: wgpu::ShaderSource::Wgsl(include_str!("shaders/static_visual.wgsl").into()) });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor { label: Some("static visual pipeline"), bind_group_layouts: &[&camera_layout, &material_layout], push_constant_ranges: &[] });
        let mut pipelines = Vec::new();
        for blend in [false, true] { for double_sided in [false, true] {
            let buffers = [
                wgpu::VertexBufferLayout { array_stride: 20, step_mode: wgpu::VertexStepMode::Vertex, attributes: &wgpu::vertex_attr_array![0 => Float32x3, 1 => Float32x2] },
                wgpu::VertexBufferLayout { array_stride: 16, step_mode: wgpu::VertexStepMode::Vertex, attributes: &wgpu::vertex_attr_array![2 => Float32x4] },
                wgpu::VertexBufferLayout { array_stride: 64, step_mode: wgpu::VertexStepMode::Instance, attributes: &wgpu::vertex_attr_array![3 => Float32x4, 4 => Float32x4, 5 => Float32x4, 6 => Float32x4] },
            ];
            pipelines.push(device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("unlit static visual"), layout: Some(&layout),
                vertex: wgpu::VertexState { module: &shader, entry_point: "vs_main", buffers: &buffers, compilation_options: Default::default() },
                fragment: Some(wgpu::FragmentState { module: &shader, entry_point: "fs_main", compilation_options: Default::default(), targets: &[Some(wgpu::ColorTargetState { format, blend: if blend { Some(wgpu::BlendState::ALPHA_BLENDING) } else { None }, write_mask: wgpu::ColorWrites::ALL })] }),
                primitive: wgpu::PrimitiveState { cull_mode: if double_sided { None } else { Some(wgpu::Face::Back) }, ..Default::default() },
                depth_stencil: Some(wgpu::DepthStencilState { format: wgpu::TextureFormat::Depth32Float, depth_write_enabled: !blend, depth_compare: wgpu::CompareFunction::LessEqual, stencil: Default::default(), bias: Default::default() }),
                multisample: Default::default(), multiview: None, cache: None,
            }));
        } }
        let texture_view = |image: &FloatImage| {
            let extent = wgpu::Extent3d { width: image.width, height: image.height, depth_or_array_layers: 1 };
            let texture = device.create_texture(&wgpu::TextureDescriptor { label: Some("static visual linear float RGBA"), size: extent, mip_level_count: 1, sample_count: 1, dimension: wgpu::TextureDimension::D2, format: wgpu::TextureFormat::Rgba32Float, usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST, view_formats: &[] });
            queue.write_texture(wgpu::ImageCopyTexture { texture: &texture, mip_level: 0, origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All }, bytemuck::cast_slice(&image.pixels), wgpu::ImageDataLayout { offset: 0, bytes_per_row: Some(image.width * 16), rows_per_image: Some(image.height) }, extent);
            texture.create_view(&Default::default())
        };
        let image_views: Vec<_> = images.iter().map(texture_view).collect();
        let white_view = texture_view(&FloatImage { width: 1, height: 1, pixels: vec![[1.; 4]] });
        let materials: Vec<_> = scene.materials.iter().map(|material| {
            let (mode, cutoff) = match material.alpha_mode { StaticAlphaMode::Opaque => (0, 0.), StaticAlphaMode::Mask { cutoff } => (1, cutoff), StaticAlphaMode::Blend => (2, 0.) };
            let uniform = MaterialUniform { factor: material.base_color, cutoff, mode, padding: [0; 2] };
            let buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("static material"), contents: bytemuck::bytes_of(&uniform), usage: wgpu::BufferUsages::UNIFORM });
            let view = material.texture_index.map(|t| &image_views[scene.textures[t].image_index]).unwrap_or(&white_view);
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor { label: Some("static material"), layout: &material_layout, entries: &[wgpu::BindGroupEntry { binding: 0, resource: buffer.as_entire_binding() }, wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(view) }] });
            GpuMaterial { group, pipeline: usize::from(mode == 2) * 2 + usize::from(material.double_sided), blend: mode == 2 }
        }).collect();
        let create_vertex = |label, bytes: &[u8]| device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some(label), contents: bytes, usage: wgpu::BufferUsages::VERTEX });
        let mut placements = vec![Vec::new(); scene.meshes.len()];
        for instance in &scene.instances { placements[instance.mesh_index].push(Mat4::from_cols_array_2d(&instance.matrix)); }
        let mut meshes = Vec::new();
        for (mesh, matrices) in scene.meshes.iter().zip(placements) {
            let vertices: Vec<_> = mesh.positions.iter().zip(&mesh.uvs).map(|(&position, &uv)| Vertex { position, uv }).collect();
            let packed: Vec<_> = matrices.iter().map(Mat4::to_cols_array_2d).collect();
            let identity = [Mat4::IDENTITY.to_cols_array_2d()];
            let instances = create_vertex("static shared instances", bytemuck::cast_slice(if packed.is_empty() { &identity[..] } else { &packed }));
            let white = mesh.primitives.iter().any(|p| p.colors.is_none()).then(|| create_vertex("static white vertex RGBA", bytemuck::cast_slice(&vec![[1f32; 4]; vertices.len()])));
            let primitives = mesh.primitives.iter().map(|p| {
                let mut min = DVec3::splat(f64::INFINITY);
                let mut max = DVec3::splat(f64::NEG_INFINITY);
                for &i in &p.indices { let v = DVec3::from_array(mesh.positions[i as usize].map(f64::from)); min = min.min(v); max = max.max(v); }
                GpuPrimitive {
                    indices: device.create_buffer_init(&wgpu::util::BufferInitDescriptor { label: Some("static primitive indices"), contents: bytemuck::cast_slice(&p.indices), usage: wgpu::BufferUsages::INDEX }),
                    count: p.indices.len() as u32, colors: p.colors.as_ref().map(|c| create_vertex("static primitive RGBA", bytemuck::cast_slice(c))), material: p.material_index, center: (min + max) * 0.5,
                }
            }).collect();
            meshes.push(GpuMesh { vertices: create_vertex("static shared vertex pool", bytemuck::cast_slice(&vertices)), white, instances, matrices: matrices.iter().map(|m| m.as_dmat4()).collect(), primitives });
        }
        Ok(Self { bounds: scene.bounds, stats, meshes, materials, pipelines, camera, camera_group, blend_draws: Vec::new(), camera_prepared: false })
    }
    /// Recompute stable back-to-front primitive-instance order in view space.
    /// Requires a right-handed view with negative view-Z forward, and ordinary
    /// WebGPU near-zero/far-one depth. Finiteness is checked; this camera convention
    /// remains the caller's responsibility.
    /// Primitive centers cannot resolve intersecting or within-primitive transparency.
    pub fn prepare_camera(&mut self, queue: &wgpu::Queue, view: Mat4, projection: Mat4) -> Result<()> {
        self.camera_prepared = false;
        ensure!(view.is_finite() && projection.is_finite(), "static camera matrices must be finite");
        let vp = projection * view;
        ensure!(vp.is_finite(), "static camera view-projection overflow");
        let view = view.as_dmat4();
        let mut draws = Vec::new();
        for (m, mesh) in self.meshes.iter().enumerate() { for (p, primitive) in mesh.primitives.iter().enumerate() {
            if self.materials[primitive.material].blend {
                for (i, matrix) in mesh.matrices.iter().enumerate() {
                    let depth = (view * *matrix * primitive.center.extend(1.)).z;
                    ensure!(depth.is_finite(), "static camera primitive depth overflow");
                    draws.push(BlendDraw { mesh: m, primitive: p, instance: i as u32, depth });
                }
            }
        } }
        draws.sort_by(|a, b| if a.depth == b.depth { std::cmp::Ordering::Equal } else { a.depth.total_cmp(&b.depth) });
        queue.write_buffer(&self.camera, 0, bytemuck::cast_slice(&vp.to_cols_array()));
        self.blend_draws = draws;
        self.camera_prepared = true;
        Ok(())
    }
    /// Encode into a caller-owned matching color/depth pass. Does not submit or clear.
    pub fn draw<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>) -> Result<()> {
        ensure!(self.camera_prepared, "static visual camera must be successfully prepared before drawing");
        pass.set_bind_group(0, &self.camera_group, &[]);
        for (m, mesh) in self.meshes.iter().enumerate() { for (p, primitive) in mesh.primitives.iter().enumerate() {
            if !self.materials[primitive.material].blend && !mesh.matrices.is_empty() { self.draw_primitive(pass, m, p, 0..mesh.matrices.len() as u32); }
        } }
        for draw in &self.blend_draws { self.draw_primitive(pass, draw.mesh, draw.primitive, draw.instance..draw.instance + 1); }
        Ok(())
    }
    fn draw_primitive<'a>(&'a self, pass: &mut wgpu::RenderPass<'a>, m: usize, p: usize, instances: std::ops::Range<u32>) {
        let mesh = &self.meshes[m]; let primitive = &mesh.primitives[p]; let material = &self.materials[primitive.material];
        pass.set_pipeline(&self.pipelines[material.pipeline]);
        pass.set_bind_group(1, &material.group, &[]);
        pass.set_vertex_buffer(0, mesh.vertices.slice(..));
        pass.set_vertex_buffer(1, primitive.colors.as_ref().or(mesh.white.as_ref()).expect("prepared primitive RGBA").slice(..));
        pass.set_vertex_buffer(2, mesh.instances.slice(..));
        pass.set_index_buffer(primitive.indices.slice(..), wgpu::IndexFormat::Uint32);
        pass.draw_indexed(0..primitive.count, 0, instances);
    }
    pub fn bounds(&self) -> &Bounds { &self.bounds }
    pub fn stats(&self) -> VisualGpuStats { self.stats }
}
