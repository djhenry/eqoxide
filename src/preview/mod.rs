//! Dedicated offline window inspection of the common static visual artifact.
mod camera;
mod http;
mod state;
use anyhow::{ensure, Context, Result};
use camera::Camera;
use eqoxide_assets::static_visual::DecodeLimits;
use eqoxide_renderer::static_visual::{GpuStaticVisual, PreparedStaticVisual, UploadLimits};
use http::Shared;
use serde_json::json;
use state::{CaptureRequest, Failure, Model, Phase, RawFrame, Submission, CAPTURE_MAX_DIMENSION};
use std::{
    io::Read,
    net::TcpListener,
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    time::{Duration, Instant},
};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop},
    window::{Window, WindowId},
};

pub struct PreviewOptions {
    pub api_port: Option<u16>,
}
/// Run the common static visual inspector without configuration or a game session.
pub fn run(path: PathBuf, options: PreviewOptions) -> Result<()> {
    ensure!(
        options.api_port != Some(0),
        "explicit preview API port must be between 1 and 65535"
    );
    let limits = DecodeLimits::default();
    let mut file = std::fs::File::open(&path).context("open preview GLB")?;
    ensure!(
        file.metadata()?.len() <= limits.max_glb_bytes as u64,
        "preview GLB exceeds encoded byte limit"
    );
    let mut bytes = Vec::new();
    (&mut file)
        .take(limits.max_glb_bytes as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= limits.max_glb_bytes,
        "preview GLB grew beyond encoded byte limit"
    );
    let prepared = PreparedStaticVisual::decode(&bytes, &limits, &UploadLimits::default())?;
    let camera = Camera::frame(prepared.bounds())?;
    let min = glam::DVec3::from_array(prepared.bounds().min.map(f64::from));
    let max = glam::DVec3::from_array(prepared.bounds().max.map(f64::from));
    let radius = ((max - min).length() * 0.5).max(0.5) as f32;
    camera.matrices(4. / 3., radius)?;
    let h = prepared.header();
    let s = prepared.stats();
    let inspection = json!({"source":path.file_name().context("preview input has no filename")?.to_string_lossy(),
        "header":{"schema_version":h.schema_version,"role":h.role,"coordinate_profile":h.coordinate_profile,"unit_scale":h.unit_scale,"bake_revision":h.bake_revision,"requirements":{"reader_version":h.requirements.reader_version,"capabilities":h.requirements.capabilities}},
        "bounds":{"min":prepared.bounds().min,"max":prepared.bounds().max},
        "stats":{"mesh_count":s.mesh_count,"shared_vertex_buffers":s.shared_vertex_buffers,"index_buffers":s.index_buffers,"color_buffers":s.color_buffers,"instance_records":s.instance_records,"primitive_count":s.primitive_count,"image_count":s.image_count,"texture_count":s.texture_count,"gpu_bytes":s.gpu_bytes,"draw_commands":s.draw_commands,"scene_triangle_instances":s.scene_triangle_instances}});
    let listener =
        TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, options.api_port.unwrap_or(0)))
            .context("bind exact preview API port")?;
    listener.set_nonblocking(true)?;
    let port = listener.local_addr()?.port();
    let event_loop = EventLoop::<()>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let mut model = Model::new(camera);
    model.radius = radius;
    let shared = Shared {
        model: Arc::new(Mutex::new(model)),
        inspection: Arc::new(inspection),
        port,
        wake: Arc::new(move || proxy.send_event(()).map_err(|_| ())),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()?;
    let listener = {
        let _enter = runtime.enter();
        tokio::net::TcpListener::from_std(listener)?
    };
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let router = http::router(shared.clone());
    let server_state = shared.clone();
    let server = runtime.spawn(async move {
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await
        {
            server_state.report_runtime_failure(format!("preview HTTP server failed: {e}"));
        }
    });
    println!("API_PORT={port}");
    let mut app = PreviewApp {
        shared: shared.clone(),
        prepared: Some(prepared),
        gpu: None,
        window: None,
        radius,
        error: None,
        dirty: true,
        dragging: false,
        cursor: None,
    };
    let loop_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        event_loop.run_app(&mut app)
    }));
    if let Ok(mut m) = shared.model.lock() {
        m.stop();
    }
    app.gpu.take();
    app.window.take();
    let _ = stop.send(());
    let _ = runtime.block_on(async { tokio::time::timeout(Duration::from_secs(1), server).await });
    runtime.shutdown_timeout(Duration::from_secs(1));
    loop_result.map_err(|_| anyhow::anyhow!("preview event loop panicked"))??;
    if let Some(error) = app.error {
        anyhow::bail!(error);
    }
    if let Some(error) = shared
        .model
        .lock()
        .map_err(|_| anyhow::anyhow!("preview state poisoned"))?
        .terminal_error
        .clone()
    {
        anyhow::bail!(error);
    }
    Ok(())
}
struct Flight {
    request: CaptureRequest,
    buffer: wgpu::Buffer,
    rx: mpsc::Receiver<Result<(), wgpu::BufferAsyncError>>,
    pitch: u32,
    size: [u32; 2],
    proof: Submission,
}
struct WindowGpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    depth: wgpu::TextureView,
    visual: GpuStaticVisual,
    flight: Option<Flight>,
}
fn surface_limits(adapter: wgpu::Limits) -> wgpu::Limits {
    wgpu::Limits {
        max_texture_dimension_2d: adapter.max_texture_dimension_2d,
        ..wgpu::Limits::downlevel_defaults()
    }
}

fn surface_alpha_mode(modes: &[wgpu::CompositeAlphaMode]) -> Result<wgpu::CompositeAlphaMode> {
    modes
        .first()
        .copied()
        .context("preview surface has no supported alpha mode")
}

impl WindowGpu {
    fn init(window: Arc<Window>, prepared: PreparedStaticVisual, shared: &Shared) -> Result<Self> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window.clone())?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
        }))
        .context("no preview surface GPU adapter")?;
        let caps = surface.get_capabilities(&adapter);
        ensure!(
            caps.usages
                .contains(wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC),
            "preview surface cannot provide actual frame capture"
        );
        let format = caps
            .formats
            .into_iter()
            .find(|f| {
                matches!(
                    f,
                    wgpu::TextureFormat::Bgra8UnormSrgb | wgpu::TextureFormat::Rgba8UnormSrgb
                )
            })
            .context("preview surface has no supported sRGB format")?;
        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor {
                label: Some("static preview"),
                required_features: wgpu::Features::empty(),
                required_limits: surface_limits(adapter.limits()),
                memory_hints: Default::default(),
            },
            None,
        ))?;
        for lost in [false, true] {
            let shared = shared.clone();
            if lost {
                device.set_device_lost_callback(move |reason, message| {
                    shared.report_runtime_failure(format!(
                        "preview device lost: {reason:?}: {message}"
                    ));
                });
            } else {
                device.on_uncaptured_error(Box::new(move |e| {
                    shared.report_runtime_failure(format!("preview GPU error: {e}"));
                }));
            }
        }
        let visual = GpuStaticVisual::upload(&device, &queue, prepared, format)?;
        let size = window.inner_size();
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: surface_alpha_mode(&caps.alpha_modes)?,
            view_formats: vec![],
        };
        ensure!(
            config.width <= device.limits().max_texture_dimension_2d
                && config.height <= device.limits().max_texture_dimension_2d,
            "preview window exceeds device dimensions"
        );
        surface.configure(&device, &config);
        let depth = Self::depth(&device, &config);
        eprintln!(
            "Preview GPU: {:?}; surface {:?}",
            adapter.get_info(),
            format
        );
        Ok(Self {
            surface,
            device,
            queue,
            config,
            depth,
            visual,
            flight: None,
        })
    }
    fn depth(device: &wgpu::Device, config: &wgpu::SurfaceConfiguration) -> wgpu::TextureView {
        device
            .create_texture(&wgpu::TextureDescriptor {
                label: Some("preview depth"),
                size: wgpu::Extent3d {
                    width: config.width,
                    height: config.height,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
                view_formats: &[],
            })
            .create_view(&Default::default())
    }
    fn resize(&mut self, size: PhysicalSize<u32>) -> Result<()> {
        if size.width == 0 || size.height == 0 {
            return Ok(());
        }
        ensure!(
            size.width <= self.device.limits().max_texture_dimension_2d
                && size.height <= self.device.limits().max_texture_dimension_2d,
            "preview resize exceeds device dimensions"
        );
        self.config.width = size.width;
        self.config.height = size.height;
        self.surface.configure(&self.device, &self.config);
        self.depth = Self::depth(&self.device, &self.config);
        Ok(())
    }
    fn poll_capture(&mut self, shared: &Shared) {
        self.device.poll(wgpu::Maintain::Poll);
        let Some(flight) = self.flight.take() else {
            return;
        };
        let active = shared
            .model
            .lock()
            .map(|m| {
                m.capture_id == Some(flight.request.id) && !m.shutdown && m.phase == Phase::Ready
            })
            .unwrap_or(false);
        if !active {
            flight.buffer.unmap();
            let _ = flight.request.tx.send(Err(Failure::new(
                503,
                "capture_unavailable",
                "capture window became unavailable",
            )));
            return;
        }
        if Instant::now() >= flight.request.deadline || flight.request.tx.is_closed() {
            flight.buffer.unmap();
            let _ = flight.request.tx.send(Err(Failure::new(
                504,
                "capture_timeout",
                "capture deadline elapsed or caller canceled",
            )));
            return;
        }
        match flight.rx.try_recv() {
            Err(mpsc::TryRecvError::Empty) => self.flight = Some(flight),
            Err(_) => {
                flight.buffer.unmap();
                let _ = flight.request.tx.send(Err(Failure::new(
                    503,
                    "map_failed",
                    "GPU map callback stopped",
                )));
            }
            Ok(Err(e)) => {
                flight.buffer.unmap();
                let _ = flight
                    .request
                    .tx
                    .send(Err(Failure::new(503, "map_failed", e.to_string())));
            }
            Ok(Ok(())) => {
                let mapped = flight.buffer.slice(..).get_mapped_range();
                let mut bytes =
                    Vec::with_capacity(flight.size[0] as usize * flight.size[1] as usize * 4);
                for row in mapped
                    .chunks_exact(flight.pitch as usize)
                    .take(flight.size[1] as usize)
                {
                    bytes.extend_from_slice(&row[..flight.size[0] as usize * 4]);
                }
                drop(mapped);
                flight.buffer.unmap();
                let raw = RawFrame {
                    bytes,
                    width: flight.size[0],
                    height: flight.size[1],
                    bgra: self.config.format == wgpu::TextureFormat::Bgra8UnormSrgb,
                    proof: flight.proof,
                    deadline: flight.request.deadline,
                };
                let _ = flight.request.tx.send(Ok(raw));
            }
        }
    }
    fn render(&mut self, shared: &Shared, radius: f32) -> Result<bool> {
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            Err(wgpu::SurfaceError::Timeout) => return Ok(false),
            Err(wgpu::SurfaceError::Outdated | wgpu::SurfaceError::Lost) => {
                self.surface.configure(&self.device, &self.config);
                return Ok(false);
            }
            Err(e) => return Err(e.into()),
        };
        let camera = {
            let mut m = shared
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("preview state poisoned"))?;
            m.apply_requests(Instant::now());
            m.active.camera.clone()
        };
        let (view, projection) =
            camera.matrices(self.config.width as f32 / self.config.height as f32, radius)?;
        self.visual.prepare_camera(&self.queue, view, projection)?;
        let color = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("static preview"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &color,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.025,
                            g: 0.03,
                            b: 0.04,
                            a: 1.,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &self.depth,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            self.visual.draw(&mut pass)?;
        }
        let request = if self.flight.is_none() {
            shared
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("preview state poisoned"))?
                .capture_pending
                .take()
        } else {
            None
        };
        let capture = if let Some(r) = request {
            if Instant::now() >= r.deadline || r.tx.is_closed() {
                let _ = r.tx.send(Err(Failure::new(
                    504,
                    "capture_timeout",
                    "capture expired before submission",
                )));
                None
            } else {
                let (pitch, bytes) = match capture_layout(self.config.width, self.config.height) {
                    Ok(layout) => layout,
                    Err(error) => {
                        let _ = r.tx.send(Err(Failure::new(
                            413,
                            "capture_size_limit",
                            error.to_string(),
                        )));
                        self.queue.submit([encoder.finish()]);
                        frame.present();
                        shared
                            .model
                            .lock()
                            .map_err(|_| anyhow::anyhow!("preview state poisoned"))?
                            .publish_submission()
                            .map_err(|e| anyhow::anyhow!(e.message))?;
                        return Ok(true);
                    }
                };
                let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("preview readback"),
                    size: bytes,
                    usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                    mapped_at_creation: false,
                });
                encoder.copy_texture_to_buffer(
                    wgpu::ImageCopyTexture {
                        texture: &frame.texture,
                        mip_level: 0,
                        origin: wgpu::Origin3d::ZERO,
                        aspect: wgpu::TextureAspect::All,
                    },
                    wgpu::ImageCopyBuffer {
                        buffer: &buffer,
                        layout: wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(pitch),
                            rows_per_image: Some(self.config.height),
                        },
                    },
                    wgpu::Extent3d {
                        width: self.config.width,
                        height: self.config.height,
                        depth_or_array_layers: 1,
                    },
                );
                Some((r, buffer, pitch))
            }
        } else {
            None
        };
        self.queue.submit([encoder.finish()]);
        frame.present();
        let proof = {
            let mut m = shared
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("preview state poisoned"))?;
            m.publish_submission()
                .map_err(|e| anyhow::anyhow!(e.message))?
        };
        let Some(proof) = proof else {
            if let Some((request, _, _)) = capture {
                let _ = request.tx.send(Err(Failure::new(
                    503,
                    "shutting_down",
                    "preview is shutting down",
                )));
            }
            return Ok(true);
        };
        if let Some((request, buffer, pitch)) = capture {
            let (tx, rx) = mpsc::channel();
            buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let _ = tx.send(result);
                });
            self.flight = Some(Flight {
                request,
                buffer,
                rx,
                pitch,
                size: [self.config.width, self.config.height],
                proof,
            });
        }
        Ok(true)
    }
}
fn capture_layout(width: u32, height: u32) -> Result<(u32, u64)> {
    ensure!(
        width > 0
            && height > 0
            && width <= CAPTURE_MAX_DIMENSION
            && height <= CAPTURE_MAX_DIMENSION,
        "capture dimensions unavailable or exceed capture limit"
    );
    let row = width.checked_mul(4).context("capture row overflow")?;
    let pitch = row.checked_add(255).context("capture pitch overflow")? / 256 * 256;
    let bytes = (pitch as u64)
        .checked_mul(height as u64)
        .context("capture allocation overflow")?;
    Ok((pitch, bytes))
}
struct PreviewApp {
    shared: Shared,
    prepared: Option<PreparedStaticVisual>,
    gpu: Option<WindowGpu>,
    window: Option<Arc<Window>>,
    radius: f32,
    error: Option<String>,
    dirty: bool,
    dragging: bool,
    cursor: Option<(f64, f64)>,
}
impl PreviewApp {
    fn fail(&mut self, event_loop: &ActiveEventLoop, error: anyhow::Error) {
        let message = format!("{error:#}");
        eprintln!("Preview failed: {message}");
        if let Ok(mut m) = self.shared.model.lock() {
            m.fail(message.clone());
        }
        self.error = Some(message);
        event_loop.exit();
    }
    fn mouse(&mut self, operation: impl FnOnce(&Camera) -> Result<Camera>) -> Result<()> {
        let mut m = self
            .shared
            .model
            .lock()
            .map_err(|_| anyhow::anyhow!("preview state poisoned"))?;
        let camera = match operation(&m.active.camera).and_then(|camera| {
            camera.matrices(m.aspect(), m.radius)?;
            Ok(camera)
        }) {
            Ok(camera) => camera,
            Err(error) => {
                tracing::warn!("preview mouse camera rejected: {error}");
                return Ok(());
            }
        };
        m.mouse_camera(camera);
        self.dirty = true;
        Ok(())
    }
}
impl ApplicationHandler<()> for PreviewApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let init = (|| -> Result<()> {
            let window = Arc::new(
                event_loop.create_window(
                    Window::default_attributes()
                        .with_title("Static visual preview — unlit inspection")
                        .with_inner_size(PhysicalSize::new(800, 600))
                        .with_active(false),
                )?,
            );
            self.gpu = Some(WindowGpu::init(
                window.clone(),
                self.prepared
                    .take()
                    .context("preview scene already consumed")?,
                &self.shared,
            )?);
            let size = window.inner_size();
            self.window = Some(window);
            let mut m = self
                .shared
                .model
                .lock()
                .map_err(|_| anyhow::anyhow!("preview state poisoned"))?;
            let occluded = m.occluded;
            m.window_changed([size.width, size.height], occluded);
            Ok(())
        })();
        if let Err(e) = init {
            self.fail(event_loop, e);
        }
    }
    fn user_event(&mut self, event_loop: &ActiveEventLoop, _: ()) {
        if self.shared.model.lock().map(|m| m.shutdown).unwrap_or(true) {
            event_loop.exit();
        } else {
            self.dirty = true;
        }
    }
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let result = (|| -> Result<()> {
            match event {
                WindowEvent::CloseRequested => {
                    self.shared
                        .model
                        .lock()
                        .map_err(|_| anyhow::anyhow!("preview state poisoned"))?
                        .stop();
                    event_loop.exit();
                }
                WindowEvent::Resized(size) => {
                    if let Some(g) = &mut self.gpu {
                        g.resize(size)?;
                    }
                    let mut m = self
                        .shared
                        .model
                        .lock()
                        .map_err(|_| anyhow::anyhow!("preview state poisoned"))?;
                    let occluded = m.occluded;
                    m.window_changed([size.width, size.height], occluded);
                    self.dirty = true;
                }
                WindowEvent::Occluded(occluded) => {
                    let mut m = self
                        .shared
                        .model
                        .lock()
                        .map_err(|_| anyhow::anyhow!("preview state poisoned"))?;
                    let viewport = m.viewport;
                    m.window_changed(viewport, Some(occluded));
                    self.dirty = true;
                }
                WindowEvent::MouseInput {
                    state,
                    button: MouseButton::Left,
                    ..
                } => {
                    self.dragging = state == ElementState::Pressed;
                    self.cursor = None;
                }
                WindowEvent::CursorMoved { position, .. } => {
                    if self.dragging {
                        if let Some((x, y)) = self.cursor {
                            self.mouse(|c| {
                                c.orbit(
                                    (position.x - x) as f32 * 0.008,
                                    (position.y - y) as f32 * 0.008,
                                )
                            })?;
                        }
                    }
                    self.cursor = Some((position.x, position.y));
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    let scroll = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32 / 40.,
                    };
                    self.mouse(|c| c.zoom(scroll))?;
                }
                WindowEvent::RedrawRequested => {
                    let ready = self
                        .shared
                        .model
                        .lock()
                        .map(|m| m.phase == Phase::Ready && !m.shutdown)
                        .unwrap_or(false);
                    if ready {
                        if let Some(g) = &mut self.gpu {
                            self.dirty = !g.render(&self.shared, self.radius)?;
                        }
                    }
                }
                _ => {}
            }
            Ok(())
        })();
        if let Err(e) = result {
            self.fail(event_loop, e);
        }
    }
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let poll = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if let Some(g) = &mut self.gpu {
                g.poll_capture(&self.shared);
            }
        }));
        if poll.is_err() {
            self.fail(event_loop, anyhow::anyhow!("preview GPU readback panicked"));
            return;
        }
        let (shutdown, ready, capture) = match self.shared.model.lock() {
            Ok(mut m) => {
                m.expire_requests(Instant::now());
                (
                    m.shutdown,
                    m.phase == Phase::Ready,
                    m.capture_pending.is_some(),
                )
            }
            Err(_) => {
                event_loop.exit();
                return;
            }
        };
        if shutdown {
            event_loop.exit();
            return;
        }
        if ready && (self.dirty || capture) {
            if let Some(window) = &self.window {
                window.request_redraw();
            }
        }
        let active = self.gpu.as_ref().is_some_and(|g| g.flight.is_some()) || self.dirty || capture;
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            Instant::now() + Duration::from_millis(if active { 16 } else { 100 }),
        ));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn surface_dimensions_use_adapter_limit_without_relaxing_capture_budget() {
        let mut adapter = wgpu::Limits::downlevel_defaults();
        adapter.max_texture_dimension_2d = 8192;
        let requested = surface_limits(adapter.clone());
        assert_eq!(requested.max_texture_dimension_2d, 8192);
        let baseline = wgpu::Limits::downlevel_defaults();
        assert_eq!(requested.max_buffer_size, baseline.max_buffer_size);
        assert_eq!(requested.max_bind_groups, baseline.max_bind_groups);
        let mut model = Model::new(Camera {
            eye: [0., -4., 2.],
            target: [0.; 3],
            fov_degrees: 60.,
        });
        model.window_changed([3000, 2400], Some(false));
        assert_eq!(model.phase, Phase::Ready);
        assert_eq!(
            model.reserve_capture(Instant::now()).err().unwrap().status,
            413
        );
        assert!(capture_layout(3000, 2400).is_err());
    }

    #[test]
    fn valid_fixture_with_busy_exact_port_fails_before_window_startup() {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            include_bytes!("../../crates/eqoxide-assets/tests/fixtures/static-visual-v1.glb"),
        )
        .unwrap();
        let error = run(
            file.path().to_path_buf(),
            PreviewOptions {
                api_port: Some(port),
            },
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("bind exact preview API port"),
            "{error:#}"
        );
    }

    #[test]
    fn capture_layout_is_checked_aligned_and_bounded() {
        assert_eq!(capture_layout(65, 3).unwrap(), (512, 1536));
        assert!(capture_layout(0, 1).is_err());
        assert!(capture_layout(2049, 1).is_err());
        assert!(capture_layout(u32::MAX, u32::MAX).is_err());
    }
    #[test]
    fn explicit_zero_port_is_rejected_before_startup() {
        assert!(run(
            PathBuf::from("missing.glb"),
            PreviewOptions { api_port: Some(0) }
        )
        .unwrap_err()
        .to_string()
        .contains("API port"));
    }
    #[test]
    fn empty_surface_alpha_capabilities_are_a_descriptive_error() {
        assert!(surface_alpha_mode(&[])
            .unwrap_err()
            .to_string()
            .contains("no supported alpha mode"));
        assert_eq!(
            surface_alpha_mode(&[wgpu::CompositeAlphaMode::Opaque]).unwrap(),
            wgpu::CompositeAlphaMode::Opaque
        );
    }
}
