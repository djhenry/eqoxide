//! Render the unified visual contract without a game account or asset service.
use anyhow::{ensure, Context, Result};
use eqoxide_assets::static_visual::{DecodeLimits, StaticRenderProfile};
use eqoxide_renderer::static_visual::{
    GpuHdrStaticVisual, GpuStaticVisual, HdrFrameLimits, PreparedStaticVisual, ReferenceLighting,
    UploadLimits,
};
use glam::{DVec3, Mat4, Vec3};
use std::{
    io::Read,
    sync::mpsc,
    time::{Duration, Instant},
};

const SIZE: u32 = 512;
enum Inspector {
    Display(Box<GpuStaticVisual>),
    Hdr(Box<GpuHdrStaticVisual>),
}

fn main() -> Result<()> {
    pollster::block_on(render())
}

async fn render() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let input = args
        .next()
        .context("usage: static_visual_render INPUT.glb OUTPUT.png")?;
    let output = args
        .next()
        .context("usage: static_visual_render INPUT.glb OUTPUT.png")?;
    ensure!(args.next().is_none(), "unexpected extra argument");
    let limits = DecodeLimits::default();
    let file = std::fs::File::open(&input).context("open static visual")?;
    ensure!(
        file.metadata()?.len() <= limits.max_glb_bytes as u64,
        "GLB exceeds read limit"
    );
    let cap = limits
        .max_glb_bytes
        .checked_add(1)
        .context("read limit overflow")?;
    let mut bytes = Vec::new();
    file.take(cap as u64).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limits.max_glb_bytes,
        "GLB exceeds read limit"
    );
    let prepared = PreparedStaticVisual::decode(&bytes, &limits, &UploadLimits::default())?;
    let stats = prepared.stats();
    let render_profile = prepared.render_profile();
    let lighting_applied = prepared.has_pbr_materials();
    let bounds = prepared.bounds().clone();
    let lo = DVec3::from_array(bounds.min.map(f64::from));
    let hi = DVec3::from_array(bounds.max.map(f64::from));
    let center = lo * 0.5 + hi * 0.5;
    let radius = ((hi - lo).length() * 0.5).max(0.5);
    let eye = center + DVec3::new(1.5, -2.0, 1.25).normalize() * radius * 3.0;
    let center = center.as_vec3();
    let eye = eye.as_vec3();
    let near = (radius * 0.01) as f32;
    let far = (radius * 8.0) as f32;
    ensure!(
        eye.is_finite()
            && center.is_finite()
            && near.is_finite()
            && far.is_finite()
            && near > 0.0
            && far > near,
        "bounds cannot be framed by the inspection camera"
    );
    let view = Mat4::look_at_rh(eye, center, Vec3::Z);
    let projection = Mat4::perspective_rh(45.0_f32.to_radians(), 1.0, near, far);

    let instance = wgpu::Instance::default();
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        })
        .await
        .context("no GPU adapter for static inspection")?;
    let adapter_info = adapter.get_info();
    let (device, queue) = adapter
        .request_device(
            &wgpu::DeviceDescriptor {
                label: Some("static visual inspection"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::MemoryUsage,
            },
            None,
        )
        .await
        .context("request inspection device")?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let visual = if prepared.profile() == StaticRenderProfile::PbrHdrBlendReferenceLight {
        let mut hdr = GpuHdrStaticVisual::upload(
            &device,
            &queue,
            prepared,
            format,
            ReferenceLighting::default(),
            &UploadLimits::default(),
            &HdrFrameLimits::default(),
        )?;
        hdr.resize(&device, SIZE, SIZE)?;
        hdr.prepare_camera(&queue, view, projection)?;
        Inspector::Hdr(Box::new(hdr))
    } else {
        let mut display = GpuStaticVisual::upload(&device, &queue, prepared, format)?;
        display.prepare_camera(&queue, view, projection)?;
        Inspector::Display(Box::new(display))
    };
    let extent = wgpu::Extent3d {
        width: SIZE,
        height: SIZE,
        depth_or_array_layers: 1,
    };
    let color = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("inspection color"),
        size: extent,
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let color_view = color.create_view(&wgpu::TextureViewDescriptor::default());
    let depth_view = if matches!(visual, Inspector::Display(_)) {
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("inspection depth"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        Some(depth.create_view(&wgpu::TextureViewDescriptor::default()))
    } else {
        None
    };
    let row_bytes = SIZE * 4;
    let padded_row =
        row_bytes.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("inspection readback"),
        size: u64::from(padded_row) * u64::from(SIZE),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let encoding_started = Instant::now();
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("inspection frame"),
    });
    match &visual {
        Inspector::Display(display) => {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("static visual material inspection"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color_view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.035,
                            g: 0.035,
                            b: 0.035,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: depth_view
                        .as_ref()
                        .context("legacy inspection depth unavailable")?,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            display.draw(&mut pass)?;
        }
        Inspector::Hdr(hdr) => hdr.encode(&mut encoder, &color_view, [0.035; 3])?,
    }
    encoder.copy_texture_to_buffer(
        wgpu::ImageCopyTexture {
            texture: &color,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::ImageCopyBuffer {
            buffer: &readback,
            layout: wgpu::ImageDataLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(SIZE),
            },
        },
        extent,
    );
    let frame_encode_cpu_ms = encoding_started.elapsed().as_secs_f64() * 1000.;
    let submit_started = Instant::now();
    queue.submit(Some(encoder.finish()));
    let slice = readback.slice(..);
    let (tx, rx) = mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device.poll(wgpu::Maintain::Wait);
    rx.recv_timeout(Duration::from_secs(10))
        .context("inspection readback callback timed out")?
        .context("map inspection pixels")?;
    let frame_submit_readback_wall_ms = submit_started.elapsed().as_secs_f64() * 1000.;
    let pixels = {
        let mapped = slice.get_mapped_range();
        let mut pixels = Vec::with_capacity((row_bytes * SIZE) as usize);
        for row in mapped.chunks_exact(padded_row as usize) {
            pixels.extend_from_slice(&row[..row_bytes as usize]);
        }
        pixels
    };
    readback.unmap();
    image::save_buffer_with_format(
        &output,
        &pixels,
        SIZE,
        SIZE,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    )
    .context("write inspection PNG")?;
    let lighting = if lighting_applied {
        "reference"
    } else {
        "unlit"
    };
    println!("mode=static-visual-gpu-inspection lighting={lighting}");
    println!("render_profile={render_profile} lighting_applied={lighting_applied}");
    println!(
        "adapter={} backend={:?}",
        adapter_info.name, adapter_info.backend
    );
    println!("stats={stats:?}");
    if let Inspector::Hdr(hdr) = &visual {
        println!("hdr_stats={:?}", hdr.stats());
    }
    println!("frame_encode_cpu_ms={frame_encode_cpu_ms:.3} frame_submit_and_readback_wall_ms={frame_submit_readback_wall_ms:.3}");
    println!(
        "bounds_server_geometry=min{:?} max{:?}",
        bounds.min, bounds.max
    );
    println!(
        "output={} width={SIZE} height={SIZE}",
        std::path::Path::new(&output).display()
    );
    Ok(())
}
