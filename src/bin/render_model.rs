/// Standalone glTF model viewer for debugging character model rendering.
///
/// This binary loads a `.glb` file and renders it in an orbit camera window,
/// using the same pipeline, shaders, and model loading code as the full EQ
/// client (`eq_renderer`). It is decoupled from the zone, HUD, networking,
/// and game state — only the rendering subsystem is used.
///
/// # Usage
///
/// ```bash
/// # Render with default "humanoid" archetype scale:
/// cargo run --release --bin render_model -- assets/models/humanoid.glb
///
/// # Specify archetype for correct scale/positioning:
/// cargo run --release --bin render_model -- assets/models/elf.glb --arch elf
/// cargo run --release --bin render_model -- assets/models/frog.glb --arch frog
/// ```
///
/// # HTTP API (port 8766)
///
/// After startup, a HTTP API is available for headless control:
///
/// - `GET  /camera`         — return current camera params (azimuth, elevation, distance)
/// - `POST /camera`         — set camera params: `{"azimuth":0,"elevation":20,"distance":30}`
/// - `GET  /frame`          — render and return a PNG screenshot
/// - `GET  /wireframe`      — return current wireframe state
/// - `POST /wireframe`      — toggle wireframe: `{"enabled":true}`
///
/// # Window Controls
///
/// - **Left-drag**: Orbit camera (azimuth + elevation)
/// - **Scroll wheel**: Zoom in/out
/// - **Close window**: Exit
///
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, WindowEvent, ElementState, MouseButton};
use winit::event_loop::{ActiveEventLoop, EventLoop};
use winit::window::{Window, WindowId, WindowAttributes};
use wgpu::util::DeviceExt;

use eq_renderer::camera;
use eq_renderer::frame_capture;
use eq_renderer::gpu::{self, GpuStaticModel, EntityUniform};
use eq_renderer::models::ModelAsset;
use eq_renderer::pipeline;

/// A colored marker at a body-part center for visual debugging.
#[derive(Clone, Debug)]
struct Marker {
    pos:    [f32; 3],
    color:  [f32; 4],
    label:  String,
}

/// Map a WLD material name to a body-part group name.
fn material_to_body_part(name: &str) -> Option<&'static str> {
    let u = name.to_uppercase();
    if u.starts_with("HOMHN") { Some("R_Arm") }
    else if u.starts_with("HOMUA") || u.starts_with("HOMFA") { Some("L_Arm") }
    else if u.starts_with("HOMCH") { Some("Torso") }
    else if u.starts_with("HOMHE") { Some("Head") }
    else if u.starts_with("HOMLG") { Some("R_Leg") }
    else if u.starts_with("HOMFT") { Some("L_Leg") }
    else if u.contains("EYE") { Some("Eyes") }
    else { None }
}

/// Body-part → marker color (RGBA, 0-1).
fn body_part_color(part: &str) -> [f32; 4] {
    match part {
        "R_Arm" => [1.0, 0.0, 0.0, 1.0],   // red
        "L_Arm" => [0.0, 1.0, 1.0, 1.0],   // cyan
        "Torso" => [0.0, 0.5, 1.0, 1.0],   // blue
        "Head"  => [0.0, 1.0, 0.0, 1.0],   // green
        "R_Leg" => [1.0, 1.0, 0.0, 1.0],   // yellow
        "L_Leg" => [1.0, 0.0, 1.0, 1.0],   // magenta
        "Eyes"  => [1.0, 0.5, 0.0, 1.0],   // orange
        _       => [0.7, 0.7, 0.7, 1.0],   // gray
    }
}

/// Compute per-material bounding boxes from the GLB and generate body-part markers.
fn compute_markers(glb_path: &Path) -> Vec<Marker> {
    let data = match std::fs::read(glb_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    // Parse GLB: skip 12-byte header, read JSON chunk.
    if data.len() < 12 { return vec![]; }
    let json_len = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let json_bytes = &data[20..20 + json_len];
    let gltf: serde_json::Value = match serde_json::from_slice(json_bytes) {
        Ok(v) => v,
        Err(_) => return vec![],
    };

    let meshes = match gltf.get("meshes").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return vec![], 
    };
    let accessors = match gltf.get("accessors").and_then(|a| a.as_array()) {
        Some(a) => a,
        None => return vec![],
    };
    let buffer_views = match gltf.get("bufferViews").and_then(|b| b.as_array()) {
        Some(b) => b,
        None => return vec![],
    };
    // GLB binary chunk starts after JSON chunk (padded to 4 bytes).
    let bin_offset = 20 + json_len + (4 - json_len % 4) % 4;
    if bin_offset + 8 > data.len() { return vec![]; }
    let bin_len = u32::from_le_bytes(data[bin_offset..bin_offset + 4].try_into().unwrap()) as usize;
    let bin_data = &data[bin_offset + 8..bin_offset + 8 + bin_len];

    let materials = gltf.get("materials").and_then(|m| m.as_array());

    // Accumulate bounds per material name.
    use std::collections::HashMap;
    let mut bounds: HashMap<String, [[f32; 3]; 2]> = HashMap::new();

    for mesh in meshes {
        let primitives = match mesh.get("primitives").and_then(|p| p.as_array()) {
            Some(p) => p,
            None => continue,
        };
        for prim in primitives {
            let mat_idx = prim.get("material").and_then(|m| m.as_u64()).unwrap_or(0) as usize;
            let mat_name = materials
                .and_then(|mats| mats.get(mat_idx))
                .and_then(|m| m.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown");

            // Get POSITION accessor — shared across all primitives in this mesh.
            let attrs = match prim.get("attributes") {
                Some(a) => a,
                None => continue,
            };
            let pos_acc_idx = match attrs.get("POSITION").and_then(|v| v.as_u64()) {
                Some(i) => i as usize,
                None => continue,
            };
            let pos_acc = &accessors[pos_acc_idx];
            let pos_bv_idx = pos_acc.get("bufferView").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let pos_bv = &buffer_views[pos_bv_idx];
            let pos_byte_offset = pos_bv.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            // Read this primitive's OWN index buffer to find which vertices it uses.
            let idx_acc_idx = match prim.get("indices").and_then(|v| v.as_u64()) {
                Some(i) => i as usize,
                None => continue,
            };
            let idx_acc = &accessors[idx_acc_idx];
            let idx_bv_idx = idx_acc.get("bufferView").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let idx_bv = &buffer_views[idx_bv_idx];
            let idx_byte_offset = idx_bv.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let idx_count = idx_acc.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let idx_component_type = idx_acc.get("componentType").and_then(|v| v.as_u64()).unwrap_or(5123) as usize;

            // Compute bounds from only the indexed vertices of this primitive.
            let mut min_pos = [f32::MAX; 3];
            let mut max_pos = [f32::MIN; 3];
            for ii in 0..idx_count {
                // Read the vertex index from the index buffer.
                let vertex_idx: usize = match idx_component_type {
                    5121 => { // UNSIGNED_BYTE
                        let off = idx_byte_offset + ii;
                        if off >= bin_data.len() { continue; }
                        bin_data[off] as usize
                    }
                    5123 => { // UNSIGNED_SHORT
                        let off = idx_byte_offset + ii * 2;
                        if off + 2 > bin_data.len() { continue; }
                        u16::from_le_bytes(bin_data[off..off + 2].try_into().unwrap()) as usize
                    }
                    5125 => { // UNSIGNED_INT
                        let off = idx_byte_offset + ii * 4;
                        if off + 4 > bin_data.len() { continue; }
                        u32::from_le_bytes(bin_data[off..off + 4].try_into().unwrap()) as usize
                    }
                    _ => continue,
                };
                // Read the position for this vertex index.
                let pos_off = pos_byte_offset + vertex_idx * 12;
                if pos_off + 12 > bin_data.len() { continue; }
                let x = f32::from_le_bytes(bin_data[pos_off..pos_off + 4].try_into().unwrap());
                let y = f32::from_le_bytes(bin_data[pos_off + 4..pos_off + 8].try_into().unwrap());
                let z = f32::from_le_bytes(bin_data[pos_off + 8..pos_off + 12].try_into().unwrap());
                min_pos[0] = min_pos[0].min(x); max_pos[0] = max_pos[0].max(x);
                min_pos[1] = min_pos[1].min(y); max_pos[1] = max_pos[1].max(y);
                min_pos[2] = min_pos[2].min(z); max_pos[2] = max_pos[2].max(z);
            }
            if min_pos[0] > max_pos[0] { continue; } // no vertices found
            let entry = bounds.entry(mat_name.to_string()).or_insert([min_pos, max_pos]);
            for i in 0..3 {
                entry[0][i] = entry[0][i].min(min_pos[i]);
                entry[1][i] = entry[1][i].max(max_pos[i]);
            }
        }
    }

    // Group by body part and compute markers.
    let mut part_bounds: HashMap<String, [[f32; 3]; 2]> = HashMap::new();
    for (mat_name, bbox) in &bounds {
        if let Some(part) = material_to_body_part(mat_name) {
            let entry = part_bounds.entry(part.to_string()).or_insert(*bbox);
            for i in 0..3 {
                entry[0][i] = entry[0][i].min(bbox[0][i]);
                entry[1][i] = entry[1][i].max(bbox[1][i]);
            }
        }
    }

    let mut markers = Vec::new();
    for (part, bbox) in &part_bounds {
        let center = [
            (bbox[0][0] + bbox[1][0]) / 2.0,
            (bbox[0][1] + bbox[1][1]) / 2.0,
            (bbox[0][2] + bbox[1][2]) / 2.0,
        ];
        let color = body_part_color(part);
        markers.push(Marker {
            pos: center, color,
            label: format!("{} ({:.1},{:.1},{:.1})", part, center[0], center[1], center[2]),
        });
    }

    markers
}

/// Read per-primitive material name and center from the GLB.
/// Returns (name, center_x, center_y, center_z) for each primitive in order.
fn read_mesh_info(glb_path: &Path) -> Vec<(String, [f32; 3])> {
    let data = match std::fs::read(glb_path) {
        Ok(d) => d,
        Err(_) => return vec![],
    };
    if data.len() < 12 { return vec![]; }
    let json_len = u32::from_le_bytes(data[12..16].try_into().unwrap()) as usize;
    let json_bytes = &data[20..20 + json_len];
    let gltf: serde_json::Value = match serde_json::from_slice(json_bytes) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let meshes = match gltf.get("meshes").and_then(|m| m.as_array()) {
        Some(m) => m,
        None => return vec![],
    };
    let accessors = match gltf.get("accessors").and_then(|a| a.as_array()) {
        Some(a) => a,
        None => return vec![],
    };
    let buffer_views = match gltf.get("bufferViews").and_then(|b| b.as_array()) {
        Some(b) => b,
        None => return vec![],
    };
    let bin_offset = 20 + json_len + (4 - json_len % 4) % 4;
    if bin_offset + 8 > data.len() { return vec![]; }
    let bin_len = u32::from_le_bytes(data[bin_offset..bin_offset + 4].try_into().unwrap()) as usize;
    let bin_data = &data[bin_offset + 8..bin_offset + 8 + bin_len];
    let materials = gltf.get("materials").and_then(|m| m.as_array());

    let mut result = Vec::new();
    for mesh in meshes {
        let primitives = match mesh.get("primitives").and_then(|p| p.as_array()) {
            Some(p) => p,
            None => continue,
        };
        for prim in primitives {
            let mat_idx = prim.get("material").and_then(|m| m.as_u64()).unwrap_or(0) as usize;
            let mat_name = materials
                .and_then(|mats| mats.get(mat_idx))
                .and_then(|m| m.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("unknown")
                .to_string();

            // Read index buffer to find which vertices this primitive uses.
            let attrs = match prim.get("attributes") { Some(a) => a, None => { result.push((mat_name, [0.0; 3])); continue; } };
            let pos_acc_idx = match attrs.get("POSITION").and_then(|v| v.as_u64()) { Some(i) => i as usize, None => { result.push((mat_name, [0.0; 3])); continue; } };
            let pos_acc = &accessors[pos_acc_idx];
            let pos_bv_idx = pos_acc.get("bufferView").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let pos_bv = &buffer_views[pos_bv_idx];
            let pos_byte_offset = pos_bv.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;

            let idx_acc_idx = match prim.get("indices").and_then(|v| v.as_u64()) { Some(i) => i as usize, None => { result.push((mat_name, [0.0; 3])); continue; } };
            let idx_acc = &accessors[idx_acc_idx];
            let idx_bv_idx = idx_acc.get("bufferView").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let idx_bv = &buffer_views[idx_bv_idx];
            let idx_byte_offset = idx_bv.get("byteOffset").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let idx_count = idx_acc.get("count").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let idx_component_type = idx_acc.get("componentType").and_then(|v| v.as_u64()).unwrap_or(5123) as usize;

            let mut sum = [0.0f32; 3];
            let mut count = 0u32;
            for ii in 0..idx_count {
                let vertex_idx: usize = match idx_component_type {
                    5121 => { let off = idx_byte_offset + ii; if off >= bin_data.len() { continue; } bin_data[off] as usize }
                    5123 => { let off = idx_byte_offset + ii * 2; if off + 2 > bin_data.len() { continue; } u16::from_le_bytes(bin_data[off..off + 2].try_into().unwrap()) as usize }
                    5125 => { let off = idx_byte_offset + ii * 4; if off + 4 > bin_data.len() { continue; } u32::from_le_bytes(bin_data[off..off + 4].try_into().unwrap()) as usize }
                    _ => continue,
                };
                let pos_off = pos_byte_offset + vertex_idx * 12;
                if pos_off + 12 > bin_data.len() { continue; }
                sum[0] += f32::from_le_bytes(bin_data[pos_off..pos_off + 4].try_into().unwrap());
                sum[1] += f32::from_le_bytes(bin_data[pos_off + 4..pos_off + 8].try_into().unwrap());
                sum[2] += f32::from_le_bytes(bin_data[pos_off + 8..pos_off + 12].try_into().unwrap());
                count += 1;
            }
            let center = if count > 0 { [sum[0]/count as f32, sum[1]/count as f32, sum[2]/count as f32] } else { [0.0; 3] };
            result.push((mat_name, center));
        }
    }
    result
}

type FrameReq = Arc<Mutex<Option<oneshot::Sender<Vec<u8>>>>>;
type SharedCamera = Arc<Mutex<SharedCameraState>>;
type SharedWireframe = Arc<Mutex<bool>>;
type SharedWindow = Arc<Mutex<Option<Arc<Window>>>>;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct SharedCameraState {
    azimuth:   f32,
    elevation: f32,
    distance:  f32,
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "--help" || args[0] == "-h" {
        eprintln!("Usage: render_model <model.glb> [--arch <archetype>] [--port <port>] [--markers] [--parts]");
        eprintln!();
        eprintln!("Standalone glTF model viewer for debugging character model rendering.");
        eprintln!("Orbit camera: left-drag to rotate, scroll to zoom.");
        eprintln!();
        eprintln!("Options:");
        eprintln!("  --markers    Render colored cubes at body-part centers with labels");
        eprintln!("  --parts      Exploded view: render each primitive side-by-side");
        eprintln!();
        eprintln!("HTTP API (default port 8766):");
        eprintln!("  GET  /camera       — current camera state");
        eprintln!("  POST /camera       — set camera: {{\"azimuth\":0,\"elevation\":20,\"distance\":30}}");
        eprintln!("  GET  /frame        — render and return PNG screenshot");
        eprintln!("  GET  /wireframe    — current wireframe state");
        eprintln!("  POST /wireframe    — toggle wireframe: {{\"enabled\":true}}");
        eprintln!();
        eprintln!("Archetypes: humanoid, elf, dwarf, gnoll, skeleton, zombie,");
        eprintln!("            creature, bear, rat, snake, frog, wasp, wolf, bat,");
        eprintln!("            bird, worm, fish");
        std::process::exit(0);
    }

    let model_path = PathBuf::from(&args[0]);
    let arch_name = args.iter().position(|a| a == "--arch")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .unwrap_or_else(|| "humanoid".to_string());
    let port: u16 = args.iter().position(|a| a == "--port")
        .and_then(|i| args.get(i + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(8766);
    let show_markers = args.contains(&"--markers".to_string());
    let parts_mode   = args.contains(&"--parts".to_string());

    let frame_req:     FrameReq         = Arc::new(Mutex::new(None));
    // Default to azimuth 180 so the viewer faces the character's FRONT. Models face
    // -X at heading 0 (the follow camera sits behind the player in-game, so you
    // normally see the back); for inspecting a model we want the front.
    let shared_camera: SharedCamera     = Arc::new(Mutex::new(SharedCameraState { azimuth: 180.0, elevation: 20.0, distance: 30.0 }));
    let shared_wire:   SharedWireframe  = Arc::new(Mutex::new(false));
    let shared_window: SharedWindow     = Arc::new(Mutex::new(None));

    // Spawn HTTP server.
    {
        let cam = shared_camera.clone();
        let wf  = shared_wire.clone();
        let fr  = frame_req.clone();
        let win = shared_window.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("http tokio runtime");
            rt.block_on(async move {
                use axum::extract::State;
                use axum::http::StatusCode;
                use axum::routing::get;
                use axum::{Json, Router};

                #[derive(Clone)]
                struct HttpState {
                    camera:    SharedCamera,
                    wireframe: SharedWireframe,
                    frame_req: FrameReq,
                    window:    SharedWindow,
                }
                let state = HttpState { camera: cam, wireframe: wf, frame_req: fr, window: win };

                async fn get_camera(State(s): State<HttpState>) -> Json<SharedCameraState> {
                    Json(s.camera.lock().unwrap().clone())
                }

                #[derive(serde::Deserialize)]
                struct CameraBody { azimuth: Option<f32>, elevation: Option<f32>, distance: Option<f32> }

                async fn post_camera(
                    State(s): State<HttpState>,
                    Json(body): Json<CameraBody>,
                ) -> StatusCode {
                    let mut cam = s.camera.lock().unwrap();
                    if let Some(az) = body.azimuth   { cam.azimuth = az; }
                    if let Some(el) = body.elevation { cam.elevation = el.clamp(-89.0, 89.0); }
                    if let Some(d)  = body.distance  { cam.distance = d.max(0.5); }
                    StatusCode::OK
                }

                async fn get_frame(State(s): State<HttpState>) -> Result<Vec<u8>, StatusCode> {
                    let (tx, rx) = oneshot::channel();
                    *s.frame_req.lock().unwrap() = Some(tx);
                    // Trigger a redraw so the frame is rendered and captured.
                    if let Ok(guard) = s.window.lock() {
                        if let Some(win) = guard.as_ref() {
                            win.request_redraw();
                        }
                    }
                    rx.await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
                }

                async fn get_wireframe(State(s): State<HttpState>) -> Json<serde_json::Value> {
                    let wf = *s.wireframe.lock().unwrap();
                    Json(serde_json::json!({ "enabled": wf }))
                }

                #[derive(serde::Deserialize)]
                struct WireframeBody { enabled: bool }

                async fn post_wireframe(
                    State(s): State<HttpState>,
                    Json(body): Json<WireframeBody>,
                ) -> StatusCode {
                    *s.wireframe.lock().unwrap() = body.enabled;
                    StatusCode::OK
                }

                let app = Router::new()
                    .route("/camera", get(get_camera).post(post_camera))
                    .route("/frame", get(get_frame))
                    .route("/wireframe", get(get_wireframe).post(post_wireframe))
                    .with_state(state);

                let addr = format!("127.0.0.1:{port}");
                let listener = match tokio::net::TcpListener::bind(&addr).await {
                    Ok(l) => l,
                    Err(e) => { eprintln!("render_model: HTTP bind {addr}: {e}"); return; }
                };
                eprintln!("render_model: HTTP API at http://{addr}");
                if let Err(e) = axum::serve(listener, app).await {
                    eprintln!("render_model: HTTP error: {e}");
                }
            });
        });
    }

    let event_loop = EventLoop::new().expect("event loop");
    let mut app = ModelViewerApp::new(model_path, arch_name, shared_camera, shared_wire, frame_req, shared_window, show_markers, parts_mode);
    event_loop.run_app(&mut app).expect("event loop run");
}

struct ModelViewerApp {
    model_path:     PathBuf,
    arch_name:      String,
    shared_camera:  SharedCamera,
    shared_wire:    SharedWireframe,
    frame_req:      FrameReq,
    shared_window:  SharedWindow,
    show_markers:   bool,
    parts_mode:     bool,
    state:          Option<ViewerState>,
}

struct ViewerState {
    window:          Arc<Window>,
    device:          wgpu::Device,
    queue:           wgpu::Queue,
    surface:         wgpu::Surface<'static>,
    surface_config:  wgpu::SurfaceConfiguration,
    pipelines:       pipeline::Pipelines,
    wireframe_pipeline: wgpu::RenderPipeline,
    camera_uniform:  pipeline::CameraUniform,
    fallback_bg:     wgpu::BindGroup,
    depth_view:      wgpu::TextureView,
    model:           GpuStaticModel,
    arch_scale:      f32,
    uniform_pool:    Vec<(wgpu::Buffer, wgpu::BindGroup)>,

    // Wireframe: line-segment index buffers (one per mesh)
    wireframe_indices: Vec<(wgpu::Buffer, u32)>,

    // Body-part markers
    markers:         Vec<Marker>,
    marker_cube_vbuf: Option<wgpu::Buffer>,
    marker_cube_ibuf: Option<wgpu::Buffer>,
    marker_uniforms:  Vec<(wgpu::Buffer, wgpu::BindGroup)>,

    // Parts mode: per-mesh material names and bounds
    parts_mode:      bool,
    mesh_names:      Vec<String>,
    mesh_centers:    Vec<[f32; 3]>,

    // Orbit camera state
    azimuth:   f32,
    elevation: f32,
    distance:  f32,
    dragging:  bool,

    // Shared with HTTP thread
    shared_camera: SharedCamera,
    shared_wire:   SharedWireframe,
    frame_req:     FrameReq,
}

impl ModelViewerApp {
    fn new(
        model_path: PathBuf, arch_name: String,
        shared_camera: SharedCamera, shared_wire: SharedWireframe, frame_req: FrameReq,
        shared_window: SharedWindow, show_markers: bool, parts_mode: bool,
    ) -> Self {
        Self { model_path, arch_name, shared_camera, shared_wire, frame_req, shared_window, show_markers, parts_mode, state: None }
    }
}

impl ApplicationHandler for ModelViewerApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.state.is_some() { return; }

        let attrs = WindowAttributes::default()
            .with_title(format!("render_model — {}", self.model_path.display()));
        let window = Arc::new(event_loop.create_window(attrs).expect("window"));

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface = instance.create_surface(window.clone()).expect("surface");
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            compatible_surface: Some(&surface),
            power_preference: wgpu::PowerPreference::HighPerformance,
            ..Default::default()
        })).expect("adapter");

        let (device, queue) = pollster::block_on(adapter.request_device(
            &wgpu::DeviceDescriptor { label: Some("render_model"), ..Default::default() },
            None,
        )).expect("device");

        let size = window.inner_size();
        let surface_caps = surface.get_capabilities(&adapter);
        let format = surface_caps.formats.iter()
            .find(|f| f.is_srgb()).copied()
            .unwrap_or(surface_caps.formats[0]);
        let surface_config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            alpha_mode: surface_caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &surface_config);

        let layouts = pipeline::build_layouts(&device);
        let camera_uniform = pipeline::build_camera_uniform(&device, &layouts);
        let fallback_bg = gpu::build_fallback_texture_bg(&device, &queue, &layouts.texture_bgl);
        let pipelines = pipeline::build_pipelines(&device, format, &layouts);
        let depth_view = gpu::create_depth_texture(&device, surface_config.width, surface_config.height);

        // Build a wireframe pipeline (LineList topology) from the character pipeline.
        // Uses explicit line-segment indices (not PolygonMode::Line, which requires
        // a GPU feature that may not be available).
        let wireframe_pipeline = {
            let vert = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("wireframe_vert"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/character.wgsl").into()),
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("wireframe_layout"),
                bind_group_layouts: &[
                    &layouts.camera_bgl,
                    &layouts.texture_bgl,
                    &layouts.entity_bgl,
                ],
                push_constant_ranges: &[],
            });
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("wireframe"),
                layout: Some(&pipeline_layout),
                vertex: wgpu::VertexState {
                    module: &vert,
                    entry_point: "vs_main",
                    buffers: &[wgpu::VertexBufferLayout {
                        array_stride: std::mem::size_of::<gpu::Vertex>() as u64,
                        step_mode: wgpu::VertexStepMode::Vertex,
                        attributes: &[
                            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 0, shader_location: 0 },
                            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x3, offset: 12, shader_location: 1 },
                            wgpu::VertexAttribute { format: wgpu::VertexFormat::Float32x2, offset: 24, shader_location: 2 },
                        ],
                    }],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &vert,
                    entry_point: "fs_main",
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: Some(wgpu::BlendState::REPLACE),
                        write_mask: wgpu::ColorWrites::ALL,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState {
                    topology: wgpu::PrimitiveTopology::LineList,
                    strip_index_format: None,
                    front_face: wgpu::FrontFace::Ccw,
                    cull_mode: None,
                    polygon_mode: wgpu::PolygonMode::Fill,
                    unclipped_depth: false,
                    conservative: false,
                },
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: wgpu::TextureFormat::Depth32Float,
                    depth_write_enabled: true,
                    depth_compare: wgpu::CompareFunction::Less,
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview: None,
                cache: None,
            })
        };

        // Load the model.
        let asset = match ModelAsset::load(&self.model_path) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("render_model: failed to load {}: {e}", self.model_path.display());
                std::process::exit(1);
            }
        };
        eprintln!("render_model: loaded {} — {} meshes, y_bottom={:.4}, y_extent={:.4}, x_center={:.4}, z_center={:.4}",
            self.model_path.display(), asset.meshes.len(),
            asset.y_bottom, asset.y_extent, asset.x_center, asset.z_center);

        let arch_scale = eq_renderer::models::archetype_scale(&self.arch_name);

        let (_, tex_bgs) = gpu::upload_textures(&device, &queue, &asset.textures, &layouts.texture_bgl);
        let tex_names: Vec<String> = asset.textures.iter().map(|t| t.name.clone()).collect();

        let meshes: Vec<gpu::GpuMesh> = asset.meshes.iter().filter_map(|mesh| {
            if mesh.positions.is_empty() || mesh.indices.is_empty() { return None; }
            let vertices: Vec<gpu::Vertex> = mesh.positions.iter().enumerate()
                .map(|(i, &p)| {
                    let nrm = mesh.normals.get(i).copied().unwrap_or([0.0, 0.0, 1.0]);
                    gpu::Vertex { position: p, normal: nrm, uv: mesh.uvs.get(i).copied().unwrap_or([0.0, 0.0]) }
                }).collect();
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None, contents: bytemuck::cast_slice(&vertices), usage: wgpu::BufferUsages::VERTEX,
            });
            let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: None, contents: bytemuck::cast_slice(&mesh.indices), usage: wgpu::BufferUsages::INDEX,
            });
            let texture_idx = mesh.texture_name.as_ref()
                .and_then(|n| tex_names.iter().position(|t| t == n));
            Some(gpu::GpuMesh { vertex_buf: vbuf, index_buf: ibuf,
                                index_count: mesh.indices.len() as u32, texture_idx,
                                base_color: mesh.base_color })
        }).collect();

        let model = GpuStaticModel {
            meshes, texture_bind_groups: tex_bgs,
            y_bottom: asset.y_bottom, y_extent: asset.y_extent,
            x_center: asset.x_center, z_center: asset.z_center,
        };

        // Pre-allocate uniform buffers (one per mesh).
        let uniform_pool: Vec<(wgpu::Buffer, wgpu::BindGroup)> = model.meshes.iter().map(|_| {
            let buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("model_uniform"),
                size: std::mem::size_of::<EntityUniform>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("model_uniform_bg"),
                layout: &layouts.entity_bgl,
                entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
            });
            (buf, bg)
        }).collect();

        // Generate wireframe index buffers: extract unique edges from triangle indices.
        let wireframe_indices: Vec<(wgpu::Buffer, u32)> = asset.meshes.iter().filter_map(|mesh| {
            if mesh.indices.is_empty() { return None; }
            let mut edge_set = std::collections::HashSet::new();
            for tri in mesh.indices.chunks_exact(3) {
                for edge in &[ (tri[0], tri[1]), (tri[1], tri[2]), (tri[2], tri[0]) ] {
                    let key = (edge.0.min(edge.1), edge.0.max(edge.1));
                    edge_set.insert(key);
                }
            }
            let mut line_indices: Vec<u32> = Vec::with_capacity(edge_set.len() * 2);
            for (a, b) in &edge_set {
                line_indices.push(*a);
                line_indices.push(*b);
            }
            let buf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("wireframe_idx"),
                contents: bytemuck::cast_slice(&line_indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            Some((buf, line_indices.len() as u32))
        }).collect();

        // Compute body-part markers from GLB data.
        let markers = if self.show_markers {
            let m = compute_markers(&self.model_path);
            for mk in &m {
                eprintln!("  marker '{}' at ({:.3}, {:.3}, {:.3})", mk.label, mk.pos[0], mk.pos[1], mk.pos[2]);
            }
            m
        } else {
            vec![]
        };

        // Create a unit cube mesh for markers (centered at origin, half-extents 0.5).
        let (marker_cube_vbuf, marker_cube_ibuf, marker_uniforms) = if !markers.is_empty() {
            #[repr(C)]
            #[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
            struct CubeVert { position: [f32; 3], normal: [f32; 3], uv: [f32; 2] }
            impl CubeVert {
                fn new(x: f32, y: f32, z: f32, nx: f32, ny: f32, nz: f32) -> Self {
                    Self { position: [x, y, z], normal: [nx, ny, nz], uv: [0.0, 0.0] }
                }
            }
            // 24 vertices (4 per face, 6 faces) with correct normals.
            let s = 0.3; // half-size of marker cube
            let verts: Vec<CubeVert> = vec![
                // Front (+Z)
                CubeVert::new(-s,-s, s, 0.0, 0.0, 1.0), CubeVert::new( s,-s, s, 0.0, 0.0, 1.0),
                CubeVert::new( s, s, s, 0.0, 0.0, 1.0), CubeVert::new(-s, s, s, 0.0, 0.0, 1.0),
                // Back (-Z)
                CubeVert::new( s,-s,-s, 0.0, 0.0,-1.0), CubeVert::new(-s,-s,-s, 0.0, 0.0,-1.0),
                CubeVert::new(-s, s,-s, 0.0, 0.0,-1.0), CubeVert::new( s, s,-s, 0.0, 0.0,-1.0),
                // Top (+Y)
                CubeVert::new(-s, s, s, 0.0, 1.0, 0.0), CubeVert::new( s, s, s, 0.0, 1.0, 0.0),
                CubeVert::new( s, s,-s, 0.0, 1.0, 0.0), CubeVert::new(-s, s,-s, 0.0, 1.0, 0.0),
                // Bottom (-Y)
                CubeVert::new(-s,-s,-s, 0.0,-1.0, 0.0), CubeVert::new( s,-s,-s, 0.0,-1.0, 0.0),
                CubeVert::new( s,-s, s, 0.0,-1.0, 0.0), CubeVert::new(-s,-s, s, 0.0,-1.0, 0.0),
                // Right (+X)
                CubeVert::new( s,-s, s, 1.0, 0.0, 0.0), CubeVert::new( s,-s,-s, 1.0, 0.0, 0.0),
                CubeVert::new( s, s,-s, 1.0, 0.0, 0.0), CubeVert::new( s, s, s, 1.0, 0.0, 0.0),
                // Left (-X)
                CubeVert::new(-s,-s,-s,-1.0, 0.0, 0.0), CubeVert::new(-s,-s, s,-1.0, 0.0, 0.0),
                CubeVert::new(-s, s, s,-1.0, 0.0, 0.0), CubeVert::new(-s, s,-s,-1.0, 0.0, 0.0),
            ];
            let indices: Vec<u32> = vec![
                0,1,2, 0,2,3,   4,5,6, 4,6,7,
                8,9,10, 8,10,11, 12,13,14, 12,14,15,
                16,17,18, 16,18,19, 20,21,22, 20,22,23,
            ];
            let vbuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("marker_cube_vbuf"),
                contents: bytemuck::cast_slice(&verts),
                usage: wgpu::BufferUsages::VERTEX,
            });
            let ibuf = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("marker_cube_ibuf"),
                contents: bytemuck::cast_slice(&indices),
                usage: wgpu::BufferUsages::INDEX,
            });
            // One uniform buffer per marker.
            let uniforms: Vec<(wgpu::Buffer, wgpu::BindGroup)> = markers.iter().map(|_| {
                let buf = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("marker_uniform"),
                    size: std::mem::size_of::<EntityUniform>() as u64,
                    usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("marker_uniform_bg"),
                    layout: &layouts.entity_bgl,
                    entries: &[wgpu::BindGroupEntry { binding: 0, resource: buf.as_entire_binding() }],
                });
                (buf, bg)
            }).collect();
            // Use the same character pipeline (it already has the right bind groups).
            (Some(vbuf), Some(ibuf), uniforms)
        } else {
            (None, None, vec![])
        };

        let visual_scale = 2.0 * asset.y_extent * arch_scale;
        let dist = visual_scale * 2.5;
        eprintln!("render_model: arch_scale={arch_scale:.2}, visual_scale={visual_scale:.2}, cam_dist={dist:.2}");

        // Read per-mesh info for parts mode.
        let mesh_info = read_mesh_info(&self.model_path);
        let mut mesh_names = Vec::new();
        let mut mesh_centers = Vec::new();
        for (i, (name, center)) in mesh_info.iter().enumerate() {
            mesh_names.push(name.clone());
            mesh_centers.push(*center);
            if self.parts_mode {
                eprintln!("  mesh {}: '{}' center=({:.3}, {:.3}, {:.3})", i, name, center[0], center[1], center[2]);
            }
        }

        // In parts mode, zoom camera out to fit all separated parts.
        let parts_distance = if self.parts_mode {
            let n = mesh_names.len().max(1) as f32;
            let spacing = 2.0 * asset.y_extent;
            n * spacing * 1.5
        } else {
            dist
        };

        self.state = Some(ViewerState {
            window: window.clone(), device, queue, surface, surface_config,
            pipelines, wireframe_pipeline, camera_uniform, fallback_bg, depth_view,
            model, arch_scale, uniform_pool, wireframe_indices,
            markers, marker_cube_vbuf, marker_cube_ibuf, marker_uniforms,
            parts_mode: self.parts_mode, mesh_names, mesh_centers,
            azimuth: 180.0, elevation: 20.0, distance: parts_distance, dragging: false,
            shared_camera: self.shared_camera.clone(),
            shared_wire: self.shared_wire.clone(),
            frame_req: self.frame_req.clone(),
        });

        // Store window handle for HTTP frame capture.
        *self.shared_window.lock().unwrap() = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(s) = &mut self.state else { return };
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(new_size) => {
                if new_size.width > 0 && new_size.height > 0 {
                    s.surface_config.width = new_size.width;
                    s.surface_config.height = new_size.height;
                    s.surface.configure(&s.device, &s.surface_config);
                    s.depth_view = gpu::create_depth_texture(&s.device, new_size.width, new_size.height);
                }
            }
            WindowEvent::MouseInput { state: btn_state, button: MouseButton::Left, .. } => {
                s.dragging = btn_state == ElementState::Pressed;
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y,
                    winit::event::MouseScrollDelta::PixelDelta(p) => p.y as f32 * 0.01,
                };
                s.distance = (s.distance * (1.0 - scroll * 0.1)).max(0.5);
                s.window.request_redraw();
            }
            WindowEvent::RedrawRequested => {
                render_frame(s);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        let Some(s) = &mut self.state else { return };
        if let DeviceEvent::MouseMotion { delta } = event {
            if s.dragging {
                s.azimuth  -= delta.0 as f32 * 0.3;
                s.elevation = (s.elevation + delta.1 as f32 * 0.3).clamp(-89.0, 89.0);
                s.window.request_redraw();
            }
        }
    }
}

/// Render one frame: camera → uniform writes → clear → draw meshes → present.
fn render_frame(s: &mut ViewerState) {
    // Sync camera from HTTP API if it was changed externally.
    {
        let http_cam = s.shared_camera.lock().unwrap();
        if (s.azimuth - http_cam.azimuth).abs() > 0.001
            || (s.elevation - http_cam.elevation).abs() > 0.001
            || (s.distance - http_cam.distance).abs() > 0.01
        {
            s.azimuth   = http_cam.azimuth;
            s.elevation  = http_cam.elevation;
            s.distance   = http_cam.distance;
        }
    }
    let wireframe = *s.shared_wire.lock().unwrap();

    let output = match s.surface.get_current_texture() {
        Ok(t) => t,
        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
            s.surface.configure(&s.device, &s.surface_config);
            return;
        }
        Err(e) => { eprintln!("render_model: surface error: {e}"); return; }
    };
    let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
    let mut encoder = s.device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());

    // Model matrix — same transform as the main client's entity pass.
    let visual_scale = 2.0 * s.model.y_extent * s.arch_scale;
    let mat = camera::entity_model_matrix_heading(
        [0.0, 0.0, 0.0], 0.0, visual_scale, s.arch_scale,
        [s.model.x_center, s.model.z_center], true, s.model.y_bottom,
    );

    // Orbit camera: spherical → Cartesian, looking at model center.
    let az = s.azimuth.to_radians();
    let el = s.elevation.to_radians();
    let eye = glam::Vec3::new(
        az.cos() * el.cos() * s.distance,
        az.sin() * el.cos() * s.distance,
        el.sin() * s.distance,
    );
    let lift = visual_scale * 0.5 + s.model.y_bottom * s.arch_scale;
    let target = glam::Vec3::new(0.0, 0.0, lift);

    let aspect = s.surface_config.width as f32 / s.surface_config.height as f32;
    let vp = camera::look_at_perspective(
        eye.to_array(), target.to_array(), [0.0, 0.0, 1.0], 60.0, aspect, 0.1, 1000.0,
    );
    s.queue.write_buffer(&s.camera_uniform.buf, 0, bytemuck::cast_slice(&vp));

    // Write entity uniform for each mesh.
    // In parts mode, offset each mesh along X so they render side-by-side.
    let n_meshes = s.model.meshes.len() as f32;
    let parts_spacing = if s.parts_mode { 2.0 * s.model.y_extent } else { 0.0 };
    for (i, (mesh, (buf, _))) in s.model.meshes.iter().zip(s.uniform_pool.iter()).enumerate() {
        let mesh_mat = if s.parts_mode {
            let offset_x = (i as f32 - n_meshes * 0.5) * parts_spacing;
            (glam::Mat4::from_cols_array_2d(&mat)
                * glam::Mat4::from_translation(glam::Vec3::new(offset_x, 0.0, 0.0)))
                .to_cols_array_2d()
        } else {
            mat
        };
        s.queue.write_buffer(buf, 0, bytemuck::bytes_of(&EntityUniform {
            model: mesh_mat, tint: mesh.base_color,
        }));
    }

    // Clear to dark gray background.
    {
        let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("clear"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view, resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r: 0.15, g: 0.15, b: 0.18, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &s.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Clear(1.0), store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None, occlusion_query_set: None,
        });
    }

    // Choose pipeline: wireframe or textured.
    let pipeline = if wireframe { &s.wireframe_pipeline } else { &s.pipelines.character };

    // Draw all meshes.
    {
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("model"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &view, resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                view: &s.depth_view,
                depth_ops: Some(wgpu::Operations {
                    load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store,
                }),
                stencil_ops: None,
            }),
            timestamp_writes: None, occlusion_query_set: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &s.camera_uniform.bind_group, &[]);
        pass.set_bind_group(1, &s.fallback_bg, &[]);

        let mut cur_tex: Option<usize> = None;
        for (i, mesh) in s.model.meshes.iter().enumerate() {
            pass.set_bind_group(2, &s.uniform_pool[i].1, &[]);
            if !wireframe {
                if mesh.texture_idx != cur_tex {
                    cur_tex = mesh.texture_idx;
                    let bg = match cur_tex {
                        Some(idx) if idx < s.model.texture_bind_groups.len() =>
                            &s.model.texture_bind_groups[idx],
                        _ => &s.fallback_bg,
                    };
                    pass.set_bind_group(1, bg, &[]);
                }
            }
            pass.set_vertex_buffer(0, mesh.vertex_buf.slice(..));
            if wireframe {
                if let Some((buf, count)) = s.wireframe_indices.get(i) {
                    pass.set_index_buffer(buf.slice(..), wgpu::IndexFormat::Uint32);
                    pass.draw_indexed(0..*count, 0, 0..1);
                }
            } else {
                pass.set_index_buffer(mesh.index_buf.slice(..), wgpu::IndexFormat::Uint32);
                pass.draw_indexed(0..mesh.index_count, 0, 0..1);
            }
        }
    }

    // Draw body-part markers (colored cubes).
    if !s.markers.is_empty() {
        if let (Some(vbuf), Some(ibuf)) = (&s.marker_cube_vbuf, &s.marker_cube_ibuf) {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("markers"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view, resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &s.depth_view,
                    depth_ops: Some(wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store }),
                    stencil_ops: None,
                }),
                timestamp_writes: None, occlusion_query_set: None,
            });
            pass.set_pipeline(&s.pipelines.character);
            pass.set_bind_group(0, &s.camera_uniform.bind_group, &[]);
            pass.set_bind_group(1, &s.fallback_bg, &[]);
            pass.set_vertex_buffer(0, vbuf.slice(..));
            pass.set_index_buffer(ibuf.slice(..), wgpu::IndexFormat::Uint32);

            for (i, marker) in s.markers.iter().enumerate() {
                // Position the marker at the body-part center, using the same
                // visual_scale as the model so markers align with the mesh.
                let marker_mat = camera::entity_model_matrix_heading(
                    marker.pos, 0.0, visual_scale, s.arch_scale,
                    [s.model.x_center, s.model.z_center], true, s.model.y_bottom,
                );
                s.queue.write_buffer(&s.marker_uniforms[i].0, 0, bytemuck::bytes_of(&EntityUniform {
                    model: marker_mat, tint: marker.color,
                }));
                pass.set_bind_group(2, &s.marker_uniforms[i].1, &[]);
                pass.draw_indexed(0..36, 0, 0..1);
            }
        }
    }

    // Frame capture: if a /frame request is pending, copy surface to buffer → PNG.
    let pending_tx = s.frame_req.lock().unwrap().take();
    if let Some(tx) = pending_tx {
        let w         = s.surface_config.width;
        let h         = s.surface_config.height;
        let row_pitch = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
            * ((w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        let staging = s.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame_staging"), size: (row_pitch * h) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            output.texture.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &staging,
                layout: wgpu::ImageDataLayout {
                    offset: 0, bytes_per_row: Some(row_pitch), rows_per_image: None,
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        s.queue.submit(std::iter::once(encoder.finish()));
        output.present();
        s.device.poll(wgpu::Maintain::Wait);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        s.device.poll(wgpu::Maintain::Wait);
        let png = frame_capture::encode_frame_png(
            &slice.get_mapped_range(), w, h, row_pitch, s.surface_config.format,
            Some(512),
        );
        let _ = tx.send(png);
    } else {
        s.queue.submit(std::iter::once(encoder.finish()));
        output.present();
    }
}
