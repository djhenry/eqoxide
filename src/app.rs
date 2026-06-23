//! Application window, render loop, and input handling.

use std::sync::{Arc, Mutex};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::ActiveEventLoop,
    keyboard::{KeyCode, PhysicalKey},
    window::{Window, WindowAttributes},
};

use glam::Vec4Swizzles as _;
use crate::camera_state::{lerp3, lerp_angle, CameraCmd, CameraSnapshot, CameraState};
use crate::eq_net::packet_handler::apply_packet;
use crate::eq_net::transport::AppPacket;
use crate::frame_capture::encode_frame_png;
use crate::game_state::GameState;
use crate::http::FrameReq;
use crate::renderer::EqRenderer;
use crate::scene::SceneState;
use crate::{assets, debug_zone, hud, zone_map};

/// Data produced by the background zone-load thread, ready for GPU upload on the main thread.
struct PendingLoad {
    zone_name: String,
    /// None means the S3D failed to load; use the fallback ground plane instead.
    assets:    Option<assets::ZoneAssets>,
    collision: Option<Arc<assets::Collision>>,
    zone_map:  Option<zone_map::ZoneMap>,
    zone_min:  [f32; 2],
    zone_max:  [f32; 2],
}

/// The winit `ApplicationHandler` and root of the render half. Owns the window + GPU surface, the
/// `EqRenderer`, the per-frame `SceneState`, camera state, input state, and the shared request
/// slots / packet receiver that connect it to the HTTP and EQ-network threads. Its event-loop
/// callbacks (`resumed`, `window_event`, `about_to_wait`) drive zone loading, per-frame update from
/// incoming packets, camera follow, and drawing.
pub struct App {
    // Window & GPU (initialised in `resumed`)
    window:        Option<Arc<Window>>,
    gpu:           Option<(wgpu::Surface<'static>, EqRenderer)>,
    egui_ctx:      Option<egui::Context>,
    egui_state:    Option<egui_winit::State>,
    egui_renderer: Option<egui_wgpu::Renderer>,
    // Asset paths
    assets_path:   std::path::PathBuf,
    models_path:   std::path::PathBuf,
    // Zone state
    current_zone:   String,
    loading:        bool,
    pending_reload: bool,
    /// Current loading step shown to the user while loading == true.
    load_status:    Arc<Mutex<String>>,
    /// Background thread writes completed load data here; render loop drains it.
    pending_load:   Arc<Mutex<Option<PendingLoad>>>,
    // Minimap
    zone_min:      [f32; 2],
    zone_max:      [f32; 2],
    minimap_zoom:  f32,
    minimap_full:  bool,
    zone_map:      Option<zone_map::ZoneMap>,
    // Camera & smooth position
    visual_player_pos:  [f32; 3],
    prev_logical_pos:   [f32; 3],
    last_moved_at:      std::time::Instant,
    camera:             CameraState,
    camera_cmd:         Arc<Mutex<Option<CameraCmd>>>,
    camera_snapshot:    Arc<Mutex<CameraSnapshot>>,
    camera_initialized: bool,
    last_frame_time:    std::time::Instant,
    fps_frame_count:    u32,
    fps_timer:          std::time::Instant,
    current_fps:        f32,
    // Keyboard movement
    keys_held:    std::collections::HashSet<KeyCode>,
    /// Free-fly position override in scene space [east, north, z].
    /// None = track server position; Some = keyboard-driven position.
    override_pos: Option<[f32; 3]>,
    /// Frames remaining where override_pos is protected from being cleared by key release.
    warp_cooldown: u32,
    /// Shared goto target — WASD writes here so the nav thread sends actual EQ packets.
    goto_target:  crate::http::GotoTarget,
    /// Shared request slots written by HUD buttons; the nav thread drains and sends them.
    hail:         crate::http::HailReq,
    say:          crate::http::SayReq,
    target:       crate::http::TargetReq,
    attack:       crate::http::AttackReq,
    cast:         crate::http::CastReq,
    sit:          crate::http::SitReq,
    consider:     crate::http::ConsiderReq,
    spells:       std::sync::Arc<crate::spells::SpellDb>,
    /// Text buffer for the HUD say box.
    say_buffer:   String,
    // Mouse
    drag_active:  bool,
    last_cursor:  winit::dpi::PhysicalPosition<f64>,
    /// Cursor position when LMB was pressed — used to distinguish click from drag.
    click_start:  Option<winit::dpi::PhysicalPosition<f64>>,
    /// Cached view-projection matrix from last render frame, for 3D picking.
    pick_view_proj: [[f32; 4]; 4],
    pick_cam_eye:   [f32; 3],
    pick_screen_w:  u32,
    pick_screen_h:  u32,
    // EQ state
    game_state:   GameState,
    /// Offline testzone mode — bypasses EQ server entirely.
    #[allow(dead_code)]
    testzone_mode: bool,
    scene:        SceneState,
    app_rx:       tokio::sync::mpsc::UnboundedReceiver<AppPacket>,
    // Frame capture for /frame API
    frame_req:    FrameReq,
    // Live player state for the /debug endpoint.
    player_info:  crate::http::PlayerInfo,
    warp:         crate::http::WarpReq,
    // Precomputed zone collision grid: floor grounding, camera collision, nameplate occlusion.
    // Held as Arc and also published to `shared_collision` so the nav thread can read it.
    collision:    Option<Arc<assets::Collision>>,
    /// Shared slot the nav thread reads to gate /goto movement against walls.
    shared_collision: assets::SharedCollision,
    /// Cache of the last terrain sample: (east, north, height). Avoids re-querying
    /// the grid each frame when the player hasn't moved horizontally.
    ground_cache: (f32, f32, f32),
    /// Most recent floor_z result. Used as the anchor for the next frame's floor_z query
    /// so the player's visual height is self-consistent and can't be pulled up to a bridge
    /// or ceiling just because the server placed them there.
    last_grounded_z: f32,
    /// Render position last frame [east, north, z], used to derive facing from motion.
    prev_render_pos: [f32; 3],
    /// Where the player should face (EQ degrees, 0=north) — set from movement direction.
    heading_target:  f32,
    /// Smoothed facing actually used for rendering and camera-behind placement.
    visual_heading:  f32,
    /// Vertical velocity in EQ units/s (positive = upward). Used for jump and fall physics.
    vert_vel:   f32,
    /// True when the player's feet are resting on solid geometry.
    on_ground:  bool,
    /// F10 toggles an on-screen debug overlay (heading values, coords, corrections).
    show_debug: bool,
    /// Whether the inventory/equipment window is open (toggled by the HUD button or the I key).
    show_inventory: bool,
    ui_layout: crate::ui_layout::UiLayout,
    /// Cached egui textures for spell-gem icons (spells01..07.tga). Empty until first render.
    spell_icons: Vec<egui::TextureHandle>,
    /// True once `load_spell_icons` has been attempted (avoids retrying every frame after failure).
    tried_icons: bool,
    /// Cached global UI zoom factor (min(w/1920, h/1080) / dpi) and the surface size it was computed
    /// for — recomputed only when the size changes, not every frame.
    ui_zoom: f32,
    ui_zoom_size: (u32, u32),
}

impl App {
    pub fn new(
        assets_path:     std::path::PathBuf,
        models_path:     std::path::PathBuf,
        character_name:  String,
        camera_cmd:      Arc<Mutex<Option<CameraCmd>>>,
        camera_snapshot: Arc<Mutex<CameraSnapshot>>,
        app_rx:          tokio::sync::mpsc::UnboundedReceiver<AppPacket>,
        frame_req:       FrameReq,
        goto_target:     crate::http::GotoTarget,
        hail:            crate::http::HailReq,
        say:             crate::http::SayReq,
        target:          crate::http::TargetReq,
        attack:          crate::http::AttackReq,
        cast:            crate::http::CastReq,
        sit:             crate::http::SitReq,
        consider:        crate::http::ConsiderReq,
        spells:          std::sync::Arc<crate::spells::SpellDb>,
        shared_collision: assets::SharedCollision,
        player_info:     crate::http::PlayerInfo,
        warp:            crate::http::WarpReq,
        testzone_mode:   bool,
    ) -> Self {
        let ui_layout = crate::ui_layout::UiLayout::load(&character_name);
        let mut game_state = GameState::new();
        game_state.player_name = character_name;

        if testzone_mode {
            game_state.zone_name = "testzone".to_string();
            game_state.zone_changed = true;
            eprintln!("APP: --testzone mode, will load debug zone");
        }

        App {
            window: None, gpu: None, egui_ctx: None, egui_state: None, egui_renderer: None,
            assets_path, models_path,
            current_zone: String::new(), loading: false, pending_reload: false,
            load_status:  Arc::new(Mutex::new(String::new())),
            pending_load: Arc::new(Mutex::new(None)),
            zone_min: [0.0; 2], zone_max: [0.0; 2],
            minimap_zoom: 1.0, minimap_full: false, zone_map: None,
            visual_player_pos: [0.0, 0.0, 0.0],
            prev_logical_pos:  [0.0, 0.0, 0.0],
            last_moved_at:     std::time::Instant::now(),
            camera: CameraState::new([0.0, 0.0, 0.0], 0.0),
            camera_cmd, camera_snapshot,
            camera_initialized: false,
            last_frame_time: std::time::Instant::now(),
            fps_frame_count: 0,
            fps_timer: std::time::Instant::now(),
            current_fps: 0.0,
            keys_held: std::collections::HashSet::new(),             override_pos: None, warp_cooldown: 0, goto_target,
            hail, say, target, attack, cast, sit, consider, spells, say_buffer: String::new(),
            drag_active: false, last_cursor: winit::dpi::PhysicalPosition::new(0.0, 0.0),
            click_start: None,
            pick_view_proj: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            pick_cam_eye: [0.0; 3],
            pick_screen_w: 800,
            pick_screen_h: 600,
            game_state, scene: SceneState::default(), app_rx, frame_req,
            player_info, warp, collision: None, shared_collision,
            ground_cache: (f32::NAN, f32::NAN, 0.0),
            last_grounded_z: 0.0,
            prev_render_pos: [0.0, 0.0, 0.0],
            heading_target:  0.0,
            visual_heading:  0.0,
            vert_vel:  0.0,
            on_ground: true,
            testzone_mode,
            show_debug: false,
            show_inventory: false,
            ui_layout,
            spell_icons: Vec::new(),
            tried_icons: false,
            ui_zoom: 1.0,
            ui_zoom_size: (0, 0),
        }
    }

    /// Snap a render Z to the zone floor beneath `(east, north)`. Returns `fallback`
    /// unchanged when no zone geometry is loaded or no floor vertex is nearby.
    /// Result is cached and only recomputed after ~2 units of horizontal movement.
    fn ground_z(&mut self, east: f32, north: f32, fallback: f32) -> f32 {
        let Some(col) = self.collision.as_deref() else { return fallback; };
        let (ce, cn, ch) = self.ground_cache;
        // Invalidate on horizontal movement OR when the anchor z shifted more than 3 units
        // from the cached height (player changed levels without moving much horizontally).
        if (ce - east).abs() < 2.0 && (cn - north).abs() < 2.0 && (ch - fallback).abs() < 3.0 {
            return ch;
        }
        let h = col.floor_z(east, north, fallback);
        self.ground_cache = (east, north, h);
        h
    }

    /// Cast a ray from the camera through screen pixel `cursor` and return the
    /// spawn_id of the closest entity whose bounding sphere it intersects.
    fn pick_at(&self, cursor: winit::dpi::PhysicalPosition<f64>) -> Option<u32> {
        let w = self.pick_screen_w as f32;
        let h = self.pick_screen_h as f32;
        if w < 1.0 || h < 1.0 { return None; }

        // Convert cursor to NDC [-1, 1]  (Y flipped: screen-top = NDC +1)
        let ndc_x =  2.0 * cursor.x as f32 / w - 1.0;
        let ndc_y = -2.0 * cursor.y as f32 / h + 1.0;

        // Unproject through the inverse VP to get near/far world points.
        // WGPU depth range is [0, 1]; NDC z=0 = near plane, z=1 = far plane.
        let vp = glam::Mat4::from_cols_array_2d(&self.pick_view_proj);
        if vp.determinant().abs() < 1e-9 { return None; }
        let inv = vp.inverse();

        let near_h = inv * glam::Vec4::new(ndc_x, ndc_y, 0.0, 1.0);
        let far_h  = inv * glam::Vec4::new(ndc_x, ndc_y, 1.0, 1.0);
        if near_h.w.abs() < 1e-9 || far_h.w.abs() < 1e-9 { return None; }
        let near_w = near_h.xyz() / near_h.w;
        let far_w  = far_h.xyz()  / far_h.w;

        let ray_origin = glam::Vec3::from(self.pick_cam_eye);
        let dir_unnorm = far_w - near_w;
        if dir_unnorm.length_squared() < 1e-9 { return None; }
        let ray_dir = dir_unnorm.normalize();

        // Test entities as bounding spheres in GPU world space [east, north, z].
        // Entity pos = [e.x=east, e.y=north] (game_state.rs).
        const SPHERE_R: f32 = 4.0;
        let mut best_t  = f32::MAX;
        let mut best_id = None;

        for (&id, e) in &self.game_state.entities {
            if e.dead { continue; }
            // Lift sphere center to entity mid-body height. Entity (x=east, y=north).
            let center = glam::Vec3::new(e.x, e.y, e.z + SPHERE_R * 0.75);
            let oc = ray_origin - center;
            let b  = oc.dot(ray_dir);
            let c  = oc.dot(oc) - SPHERE_R * SPHERE_R;
            let disc = b * b - c;
            if disc < 0.0 { continue; }
            let t = -b - disc.sqrt();
            if t > 0.0 && t < best_t {
                best_t  = t;
                best_id = Some(id);
            }
        }

        best_id
    }

    // ── Zone loading ──────────────────────────────────────────────────────────

    fn reload_zone(&mut self) {
        let zone_name = self.scene.zone.clone();
        if self.gpu.is_none() { self.loading = false; return; }

        self.ground_cache = (f32::NAN, f32::NAN, 0.0);
        self.vert_vel  = 0.0;
        self.on_ground = true;

        // testzone is assembled from in-memory debug data — handle it inline.
        if zone_name == "testzone" {
            if let Some((_, renderer)) = &mut self.gpu {
                renderer.upload_zone_assets(&debug_zone::make_debug_zone());
                eprintln!("renderer: debug zone loaded ({} meshes)", renderer.gpu_meshes.len());
            }
            self.loading = false;
            return;
        }

        let s3d_path    = self.assets_path.join(format!("{}.s3d", zone_name));
        let maps_dir    = self.assets_path.join("maps");
        let load_status = self.load_status.clone();
        let pending     = self.pending_load.clone();

        *load_status.lock().unwrap() = "Opening S3D archive…".to_string();

        std::thread::spawn(move || {
            let set_status = |s: &str| { *load_status.lock().unwrap() = s.to_string(); };

            set_status("Reading zone geometry…");
            let (opt_assets, zone_min, zone_max) = match assets::ZoneAssets::load_all(&s3d_path) {
                Ok(za) => {
                    let (zone_min, zone_max) = if let Some((mn, mx)) = za.bounds_xy() {
                        // bounds_xy already returns [east, north] = [server_x, server_y].
                        (mn, mx)
                    } else {
                        ([0.0f32; 2], [0.0f32; 2])
                    };
                    (Some(za), zone_min, zone_max)
                }
                Err(e) => {
                    eprintln!("renderer: zone '{}' not found ({}), using fallback", zone_name, e);
                    (None, [0.0f32; 2], [0.0f32; 2])
                }
            };

            set_status("Building collision grid…");
            let collision = opt_assets.as_ref()
                .map(|za| Arc::new(assets::Collision::build(za, 32.0)));

            set_status("Loading minimap…");
            let zone_map = zone_map::ZoneMap::load(&maps_dir, &zone_name);

            set_status("Uploading to GPU…");
            *pending.lock().unwrap() = Some(PendingLoad {
                zone_name, assets: opt_assets, collision, zone_map, zone_min, zone_max,
            });
        });
    }

    /// Called each frame to check whether the background load thread has finished.
    /// If so, does the GPU upload (must be on the main thread) and clears `loading`.
    fn maybe_finish_load(&mut self) {
        let result = self.pending_load.lock().unwrap().take();
        let Some(load) = result else { return };

        if let Some((_, renderer)) = &mut self.gpu {
            match load.assets {
                Some(ref za) => {
                    renderer.upload_zone_assets(za);
                    eprintln!("renderer: uploaded {} meshes for '{}'", renderer.gpu_meshes.len(), load.zone_name);
                }
                None => {
                    renderer.upload_zone_assets(&debug_zone::make_fallback_ground());
                }
            }
        }

        self.zone_min  = load.zone_min;
        self.zone_max  = load.zone_max;
        self.collision = load.collision.clone();
        *self.shared_collision.write().unwrap() = load.collision;
        self.zone_map  = load.zone_map;
        self.loading   = false;
        *self.load_status.lock().unwrap() = String::new();
    }

    // ── GPU initialisation ────────────────────────────────────────────────────

    fn init_gpu(&mut self, window: Arc<Window>) {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::default());
        let surface  = instance.create_surface(window.clone()).expect("create surface");
        let (adapter, device, queue) = pollster::block_on(async {
            let adapter = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    compatible_surface: Some(&surface), ..Default::default()
                })
                .await.expect("no suitable GPU adapter");
            let (device, queue) = adapter
                .request_device(&wgpu::DeviceDescriptor::default(), None)
                .await.expect("request device");
            (adapter, device, queue)
        });
        let size   = window.inner_size();
        let caps   = surface.get_capabilities(&adapter);
        let format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(caps.formats[0]);
        let surface_config = wgpu::SurfaceConfiguration {
            usage:   wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            format,  width: size.width.max(1), height: size.height.max(1),
            // AutoNoVsync avoids Wayland compositor vsync timeouts when the window
            // is not actively composited (e.g. idle/minimized), which would cause
            // surface.get_current_texture() to block and time out, breaking /frame captures.
            present_mode: caps.present_modes.iter().copied()
                .find(|&m| m == wgpu::PresentMode::Mailbox)
                .unwrap_or(wgpu::PresentMode::AutoNoVsync),
            desired_maximum_frame_latency: 2,
            alpha_mode: caps.alpha_modes[0], view_formats: vec![],
        };
        surface.configure(&device, &surface_config);
        let egui_ctx      = egui::Context::default();
        let egui_state    = egui_winit::State::new(
            egui_ctx.clone(), egui::ViewportId::ROOT, &*window, None, None, None,
        );
        let egui_renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
        let mut renderer  = EqRenderer::new(device, queue, surface_config);
        renderer.load_character_models(&self.models_path, &self.assets_path);
        self.egui_ctx      = Some(egui_ctx);
        self.egui_state    = Some(egui_state);
        self.egui_renderer = Some(egui_renderer);
        self.gpu           = Some((surface, renderer));
        self.window        = Some(window);
    }

    // ── Render loop ───────────────────────────────────────────────────────────

    fn render_frame(&mut self) {
        // Drain EQ packets into game state.
        while let Ok(packet) = self.app_rx.try_recv() {
            apply_packet(&mut self.game_state, &packet);
        }

        // Process warp requests — teleport directly, bypassing collision.
        let warp_req = self.warp.lock().unwrap().take();
        if let Some((wx, wy, wz)) = warp_req {
            self.game_state.player_x = wx;
            self.game_state.player_y = wy;
            self.game_state.player_z = wz;
            self.override_pos = Some([wx, wy, wz]);
            self.visual_player_pos = [wx, wy, wz];
            self.heading_target = self.game_state.player_heading;
            self.visual_heading = self.game_state.player_heading;
            *self.goto_target.lock().unwrap() = Some((wx, wy, wz));
            eprintln!("warp: teleported to ({:.1}, {:.1}, {:.1})", wx, wy, wz);
        }

        self.scene = SceneState::from_game_state(&self.game_state);

        // Update shared player state for the /debug HTTP endpoint.
        {
            let gs = &self.game_state;
            let pos = self.override_pos.unwrap_or([gs.player_x, gs.player_y, gs.player_z]);
            let h_cw = crate::eq_net::protocol::ccw_to_cw(gs.player_heading);
            *self.player_info.lock().unwrap() = crate::http::PlayerState {
                zone:       gs.zone_name.clone(),
                race:       gs.player_race.clone(),
                class:      gs.player_class.clone(),
                level:      gs.player_level as u32,
                pos_east:   pos[0],
                pos_north:  pos[1],
                pos_up:     pos[2],
                heading_ccw: gs.player_heading,
                heading_cw:  h_cw,
                server_corrections: gs.server_corrections,
                mem_spells: gs.mem_spells,
                target_id:  gs.target_id,
            };
        }

        // In the test zone, inject fake billboards so every loaded character model
        // is rendered side-by-side for visual debugging.
        if self.scene.zone == "testzone" {
            self.scene.inject_test_billboards();
        }

        // Snap NPC billboards to terrain floor so they don't hover above geometry.
        // NPCs get z from the server spawn/update packets; the player gets floor_z
        // applied each frame. Apply the same grounding to entity billboards here.
        if let Some(col) = &self.collision {
            for b in &mut self.scene.billboards {
                b.pos[2] = col.floor_z(b.pos[0], b.pos[1], b.pos[2]);
            }
        }

        // Detect movement from the logical (server-authoritative) position.
        // Nav steps fire every 150 ms; we latch "moving" for 250 ms so the
        // walking animation runs continuously between steps rather than flickering.
        {
            let lp = [self.game_state.player_x, self.game_state.player_y, self.game_state.player_z];
            let dx = lp[0] - self.prev_logical_pos[0];
            let dy = lp[1] - self.prev_logical_pos[1];
            if dx * dx + dy * dy > 0.01 {
                self.last_moved_at = std::time::Instant::now();
            }
            self.prev_logical_pos = lp;
            // A live combat swing (OP_Animation under the player's spawn id) overrides movement so
            // her attacks animate; otherwise walk/idle from recent movement.
            let pid = self.game_state.player_id;
            let swinging = self.game_state.combat_anims.get(&pid)
                .map_or(false, |(_, t)| t.elapsed() < crate::scene::COMBAT_SWING_WINDOW);
            self.scene.player_action = if let Some((code, _)) = self.game_state.combat_anims.get(&pid).filter(|_| swinging) {
                format!("C{:02}", code)
            } else if self.last_moved_at.elapsed().as_millis() < 250 {
                "walking".to_string()
            } else {
                "idle".to_string()
            };
        }

        // Snap camera to player on first valid spawn.
        // In testzone there's no server, so init the camera immediately once the
        // zone is loaded (billboards injected, GPU ready).
        let should_init_cam = if self.scene.zone == "testzone" {
            !self.camera_initialized && self.gpu.is_some() && !self.loading
        } else {
            !self.camera_initialized && self.game_state.player_id != 0
        };
        if should_init_cam {
            self.visual_player_pos = self.scene.player_pos;
            self.heading_target    = self.scene.player_heading;
            self.visual_heading    = self.scene.player_heading;
            self.camera = CameraState::new(self.scene.player_pos, self.scene.player_heading);
            self.camera_initialized = true;
        }

        if self.scene.zone_changed && self.scene.zone != self.current_zone {
            self.loading       = true;
            self.pending_reload = true;
            self.current_zone  = self.scene.zone.clone();
        }

        let now = std::time::Instant::now();
        let dt  = (now - self.last_frame_time).as_secs_f32().min(0.1);
        self.last_frame_time = now;

        // FPS counter: average over 0.5s windows.
        self.fps_frame_count += 1;
        let fps_elapsed = self.fps_timer.elapsed().as_secs_f32();
        if fps_elapsed >= 0.5 {
            self.current_fps = self.fps_frame_count as f32 / fps_elapsed;
            self.fps_frame_count = 0;
            self.fps_timer = now;
        }

        // Classic EQ control scheme:
        //   A/D without LMB → rotate the player character (classic default: "Rotates the character")
        //   A/D with LMB held → strafe left/right (LMB engages camera-orbit mode in our client)
        //   W/S → always move forward/back in the current facing direction
        //   R → reset camera to AutoFollow and clear any keyboard override
        //
        // override_pos [east, north, z] drives the visual immediately each frame.
        // goto_target (server_x=east, server_y=north, server_z) is written so the nav
        // thread sends actual EQ position-update packets to the server.

        // Determine A/D mode before the movement block so the heading block can use it.
        let a_held = self.keys_held.contains(&KeyCode::KeyA);
        let d_held = self.keys_held.contains(&KeyCode::KeyD);
        // Rotate mode: LMB is up (not dragging camera). Strafe mode: LMB held.
        let rotating = !self.drag_active && (a_held || d_held);
        // Any manual movement key held. When true, the player's facing is driven by heading_target
        // (a/d rotation), NOT by motion direction — so strafing (Q/E, A/D+LMB) keeps facing forward
        // instead of turning to face the sideways motion (which would feed back through the
        // auto-follow camera into a spin). Motion-derived heading is only for nav-driven /goto.
        let manual_move = a_held || d_held
            || self.keys_held.contains(&KeyCode::KeyW)
            || self.keys_held.contains(&KeyCode::KeyS)
            || self.keys_held.contains(&KeyCode::KeyQ)
            || self.keys_held.contains(&KeyCode::KeyE);

        {
            // EQ character run speed is ~35 EQ-units/sec; higher values trigger server rubber-band.
            const MOVE_SPEED: f32 = 35.0;
            // Classic EQ turn speed — about 3 full rotations per second feels right.
            const TURN_SPEED: f32 = 120.0; // degrees per second

            // Rotate mode: update heading directly and keep camera snapped behind the player.
            // The world is rendered X-mirrored (the clip-space X flip in look_at_perspective that
            // un-mirrors the zone geometry), which reverses on-screen left/right. So A must DECREASE
            // heading and D increase it for rotation to LOOK correct (A = turn left on screen,
            // D = turn right). Heading itself stays EQ-CCW; only the key→direction mapping flips.
            if rotating {
                if a_held { self.heading_target = (self.heading_target - TURN_SPEED * dt).rem_euclid(360.0); }
                if d_held { self.heading_target = (self.heading_target + TURN_SPEED * dt).rem_euclid(360.0); }
                // Keep the camera in AutoFollow so it tracks the new heading each frame.
                self.camera.reset_to_follow();
            }

            // When rotating, derive forward/right from heading_target so W moves immediately
            // in the direction the player is turning toward (no 1-frame camera lag).
            // When strafing (LMB held), use the camera azimuth as before.
            let (fwd_e, fwd_n, right_e, right_n) = if rotating {
                let h = self.heading_target.to_radians();
                // EQ heading: 0=north(+Y), increases CCW (90=west). Forward unit vector
                // (east,north) = (-sin h, cos h); right is forward rotated -90°.
                (-h.sin(), h.cos(), h.cos(), h.sin())
            } else {
                let az = self.camera.azimuth;
                (-az.cos(), -az.sin(), -az.sin(), az.cos())
            };

            let mut de = 0.0_f32;
            let mut dn = 0.0_f32;
            if self.keys_held.contains(&KeyCode::KeyW) { de += fwd_e; dn += fwd_n; }
            if self.keys_held.contains(&KeyCode::KeyS) { de -= fwd_e; dn -= fwd_n; }
            // Strafe: Q = left, E = right (always); A/D strafe only while LMB (camera-orbit) is held.
            // Under the X-mirrored render, screen-left strafe moves along +right_vec and screen-right
            // along -right_vec — the same left/right reversal as the rotation fix above.
            let q_held = self.keys_held.contains(&KeyCode::KeyQ);
            let e_held = self.keys_held.contains(&KeyCode::KeyE);
            let strafe_left  = q_held || (self.drag_active && a_held);
            let strafe_right = e_held || (self.drag_active && d_held);
            if strafe_left  { de += right_e; dn += right_n; }
            if strafe_right { de -= right_e; dn -= right_n; }
            // Jump: only from solid ground.
            if self.keys_held.contains(&KeyCode::Space) && self.on_ground {
                const JUMP_VELOCITY: f32 = 13.0;
                self.vert_vel  = JUMP_VELOCITY;
                self.on_ground = false;
            }

            if de != 0.0 || dn != 0.0 {
                let len = (de * de + dn * dn).sqrt();
                de = de / len * MOVE_SPEED * dt;
                dn = dn / len * MOVE_SPEED * dt;
                let base = self.override_pos.unwrap_or(self.visual_player_pos);

                // Collision: don't let the player walk through walls. Cast at chest
                // height so low lips/stairs don't block. If the full move hits a wall,
                // try sliding along each axis so the player glides along the surface
                // instead of sticking. `clear` borrows collision immutably; NLL ends
                // that borrow before the self-field writes below.
                const PLAYER_RADIUS: f32 = 2.0;
                let chest = base[2] + 3.0;
                let col = self.collision.as_ref();
                let clear = |mde: f32, mdn: f32| -> bool {
                    match col {
                        Some(c) => c.path_clear(
                            [base[0], base[1], chest],
                            [base[0] + mde, base[1] + mdn, chest],
                            PLAYER_RADIUS,
                        ),
                        None => true,
                    }
                };
                let (mde, mdn) = if clear(de, dn) {
                    (de, dn)
                } else if clear(de, 0.0) {
                    (de, 0.0)
                } else if clear(0.0, dn) {
                    (0.0, dn)
                } else {
                    eprintln!("COLLISION: WASD fully blocked at ({:.1},{:.1}) heading {:.0}° tried ({:.2},{:.2})",
                              base[0], base[1], self.heading_target, de, dn);
                    self.game_state.log_msg("collision", &format!("Blocked by wall at ({:.0},{:.0})", base[0], base[1]));
                    (0.0, 0.0) // boxed in — hold position
                };

                // Step-up: when on the ground, check if the floor at the new XY is
                // higher than the current z (ramp or stair). Use a raised anchor so
                // the ray starts above the step and can find the surface above us.
                const STEP_HEIGHT: f32 = 3.0;
                let new_e = base[0] + mde;
                let new_n = base[1] + mdn;
                // When floor_z finds no geometry it returns the fallback unchanged.
                // Guard against that: if step_floor == step_fallback the ray missed and
                // we must NOT step up, otherwise the player gets launched into the sky
                // one STEP_HEIGHT per frame whenever they walk over a gap in the mesh.
                let step_fallback = base[2] + STEP_HEIGHT;
                let step_floor = if self.on_ground && (mde != 0.0 || mdn != 0.0) {
                    self.ground_z(new_e, new_n, step_fallback)
                } else {
                    base[2]
                };
                let geometry_hit = (step_floor - step_fallback).abs() > 0.05;
                let new_z = if self.on_ground
                    && geometry_hit
                    && step_floor > base[2] + 0.1
                    && step_floor - base[2] <= STEP_HEIGHT
                {
                    step_floor
                } else {
                    base[2]
                };

                let new_pos = [new_e, new_n, new_z];
                self.override_pos = Some(new_pos);
                // Move camera focus with the player regardless of camera mode
                // (ManualOrbit keeps focus fixed otherwise, so the player walks away).
                self.camera.focus = new_pos;
                // override/world pos = [east, north, z] = [server_x, server_y, server_z];
                // goto_target is in server coords (server_x, server_y, server_z) — no swap.
                *self.goto_target.lock().unwrap() = Some((new_pos[0], new_pos[1], new_pos[2]));
            } else if self.override_pos.is_some() && self.on_ground {
                // Keys released while on ground: drop the visual override so server position takes over.
                self.override_pos = None;
            }
        }

        // Lerp visual position toward the logical position so nav steps (150 ms / 15 units)
        // glide rather than pop. Snap on teleports (>100 XY units gap).
        // Z is intentionally excluded from the lerp: server z (gs.player_z) is the spawn z
        // and is never updated during movement, so lerping toward it would pull the player
        // up into balconies/ceilings. Ground snap below is the sole authority on visual height.
        // When a keyboard override is active, use it directly instead of following the server.
        if let Some(op) = self.override_pos {
            self.visual_player_pos = op;
            self.scene.player_pos  = op;
        } else {
            let target = self.scene.player_pos;
            let dx = target[0] - self.visual_player_pos[0];
            let dy = target[1] - self.visual_player_pos[1];
            let xy_dist = (dx * dx + dy * dy).sqrt();
            if xy_dist > 100.0 {
                // Large XY teleport: snap position including z so ground snap initializes correctly.
                self.visual_player_pos = target;
            } else if xy_dist > 0.01 {
                let alpha = 1.0 - (-15.0_f32 * dt).exp();
                self.visual_player_pos[0] += (target[0] - self.visual_player_pos[0]) * alpha;
                self.visual_player_pos[1] += (target[1] - self.visual_player_pos[1]) * alpha;
                // Z not lerped — ground snap owns it.
            }
            self.scene.player_pos = self.visual_player_pos;
        }

        // Vertical physics: fall under gravity, land on geometry, jump on spacebar.
        // Replaces the old static ground-snap. The floor query uses the player's current z
        // as anchor so balconies and ceilings above never read as the floor.
        {
            const GRAVITY: f32       = 20.0; // EQ units/s²
            const MAX_FALL: f32      = 50.0; // EQ units/s terminal velocity

            let p = self.scene.player_pos; // [east, north, z]
            // floor_z with anchor = p[2]: ray_start = p[2]+2, finds surfaces at or below that.
            // Balconies/ceilings above p[2]+2 have negative t and are never returned.
            let floor = self.ground_z(p[0], p[1], p[2]);

            let new_z = if self.on_ground {
                if (floor - p[2]).abs() <= 0.5 {
                    // Normal ground tracking: stay snapped to floor surface.
                    floor
                } else if floor > p[2] + 0.5 {
                    // Floor is above us (edge case from geometry). Snap up.
                    floor
                } else {
                    // Floor dropped away — walked off a ledge; start falling.
                    self.on_ground = false;
                    p[2]
                }
            } else {
                // Airborne: integrate gravity.
                self.vert_vel -= GRAVITY * dt;
                self.vert_vel  = self.vert_vel.max(-MAX_FALL);
                let candidate  = p[2] + self.vert_vel * dt;
                if candidate <= floor {
                    // Landed.
                    self.vert_vel  = 0.0;
                    self.on_ground = true;
                    floor
                } else {
                    candidate
                }
            };

            if self.on_ground { self.last_grounded_z = new_z; }
            self.scene.player_pos[2]   = new_z;
            self.camera.focus[2]       = new_z;
            self.visual_player_pos[2]  = new_z;
            // Keep override_pos z in sync so the next WASD base starts at the right height.
            if let Some(ref mut op) = self.override_pos { op[2] = new_z; }
        }

        // Face the direction of travel. Server position updates for the player carry
        // no heading, so derive it from frame-to-frame motion and smooth it. The camera
        // sits behind this heading, so turning the character also swings the view.
        {
            let de = self.scene.player_pos[0] - self.prev_render_pos[0]; // east
            let dn = self.scene.player_pos[1] - self.prev_render_pos[1]; // north
            // Only derive heading from motion for NAV-driven movement (/goto), which carries no
            // keyboard heading. For any manual movement, the facing is heading_target (set by a/d) —
            // so strafing keeps facing forward instead of turning toward the sideways motion (which
            // would swing the auto-follow camera and spin the view).
            if !manual_move && de * de + dn * dn > 0.02 {
                let motion_deg = (-de).atan2(dn).to_degrees().rem_euclid(360.0);
                // Guard against ~180° flips caused by the backward position-correction lerp
                // that occurs when W is released and visual_player_pos snaps back toward the
                // server position (which lags up to ~5 units behind the keyboard override).
                // Legitimate heading changes per frame (forward motion, nav corners) are
                // never near 180° from the current facing.
                let diff = (motion_deg - self.visual_heading).rem_euclid(360.0);
                if diff <= 90.0 || diff >= 270.0 {
                    self.heading_target = motion_deg;
                }
            }
            self.prev_render_pos = self.scene.player_pos;
            // When rotating with A/D, snap visual_heading immediately for responsive feel.
            // When following motion, lerp smoothly to avoid jitter from nav steps.
            if rotating {
                self.visual_heading = self.heading_target;
            } else {
                let alpha = 1.0 - (-10.0_f32 * dt).exp();
                let cur = self.visual_heading.to_radians();
                let tgt = self.heading_target.to_radians();
                self.visual_heading = lerp_angle(cur, tgt, alpha).to_degrees().rem_euclid(360.0);
            }
            self.scene.player_heading = self.visual_heading;
        }

        if let Ok(mut lock) = self.camera_cmd.lock() {
            if let Some(cmd) = lock.take() { self.camera.apply_cmd(cmd); }
        }
        let (mut cam_eye, cam_target) = self.camera.tick(dt, self.scene.player_pos, self.scene.player_heading);
        // Camera collision: iteratively pull the eye toward the target until
        // the segment is clear.  A single pass fails in multi-story buildings
        // where the eye lands between two floor slabs.
        if let Some(col) = self.collision.as_deref() {
            for _ in 0..5 {
                if let Some(t) = col.nearest_hit_t(cam_target, cam_eye) {
                    let frac = (t * 0.85).clamp(0.05, 1.0);
                    let new_eye = lerp3(cam_target, cam_eye, frac);
                    if new_eye == cam_eye { break; }
                    cam_eye = new_eye;
                } else {
                    break;
                }
            }
        }
        if let Ok(mut snap) = self.camera_snapshot.lock() { *snap = self.camera.snapshot(); }

        // ── GPU work: renderer + egui share a command encoder ─────────────────
        // Use direct field access (not method calls on self) while the GPU
        // borrow is live so Rust can verify field-level disjointness.
        let Some((surface, renderer)) = &mut self.gpu else { return };

        let output = match surface.get_current_texture() {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                surface.configure(&renderer.device, &renderer.surface_config);
                return;
            }
            Err(wgpu::SurfaceError::Timeout) => return, // compositor throttling; retry next frame
            Err(e) => { eprintln!("surface error: {e}"); return; }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = renderer.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("frame") },
        );

        renderer.render_frame(&mut enc, &view, &self.scene, cam_eye, cam_target, dt);

        // Cache picking data for the next mouse-click query.
        self.pick_view_proj = renderer.last_view_proj;
        self.pick_cam_eye   = renderer.last_cam_pos;
        self.pick_screen_w  = renderer.surface_config.width;
        self.pick_screen_h  = renderer.surface_config.height;

        // Lazily load spell-gem icon atlases (needs egui Context, only available at render time).
        if !self.tried_icons {
            if let Some(ctx) = &self.egui_ctx {
                self.spell_icons = hud::load_spell_icons(ctx);
                self.tried_icons = true;
            }
        }

        // Egui pass — use associated function to avoid reborrowing self.
        let load_status_text = self.load_status.lock().unwrap().clone();
        Self::egui_pass(
            &mut self.egui_state, &mut self.egui_renderer, &self.egui_ctx, &mut self.ui_layout, &self.window,
            &mut enc, &view, renderer, self.loading, &self.current_zone, &load_status_text,
            &self.scene, self.zone_min, self.zone_max,
            &mut self.minimap_zoom, &mut self.minimap_full,
            self.current_fps, self.zone_map.as_ref(),
            cam_eye, self.collision.as_deref(),
            &self.hail, &self.say, &self.target, &mut self.say_buffer,
            &self.attack, &self.cast, &self.sit, &self.consider, &self.spells,
            &self.spell_icons,
            &mut self.show_inventory,
            &mut self.ui_zoom, &mut self.ui_zoom_size,
            self.show_debug, self.game_state.server_corrections,
        );

        // Submit — associated function avoids reborrowing self.
        Self::submit_frame(&self.frame_req, enc, output, renderer);

        if self.loading {
            // Cap at 120 fps while loading so the background thread gets more CPU.
            let frame_budget = std::time::Duration::from_micros(8_333);
            let elapsed = now.elapsed();
            if elapsed < frame_budget {
                std::thread::sleep(frame_budget - elapsed);
            }
        }

        if let Some(w) = &self.window { w.request_redraw(); }
        // GPU borrow (renderer) is released here.
        // pending_reload is checked by window_event after render_frame returns.
    }

    /// Egui render pass. Takes fields as explicit parameters so Rust can verify
    /// they are disjoint from the caller's live `&mut renderer` borrow.
    fn egui_pass(
        egui_state:    &mut Option<egui_winit::State>,
        egui_renderer: &mut Option<egui_wgpu::Renderer>,
        egui_ctx:      &Option<egui::Context>,
        ui_layout:     &mut crate::ui_layout::UiLayout,
        window:        &Option<Arc<Window>>,
        encoder:       &mut wgpu::CommandEncoder,
        view:          &wgpu::TextureView,
        renderer:      &EqRenderer,
        loading:       bool,
        current_zone:  &str,
        load_status:   &str,
        scene:         &SceneState,
        zone_min:      [f32; 2],
        zone_max:      [f32; 2],
        minimap_zoom:  &mut f32,
        minimap_full:  &mut bool,
        current_fps:   f32,
        zone_map:      Option<&zone_map::ZoneMap>,
        cam_eye:       [f32; 3],
        collision:     Option<&assets::Collision>,
        hail:          &crate::http::HailReq,
        say:           &crate::http::SayReq,
        target:        &crate::http::TargetReq,
        say_buffer:    &mut String,
        attack:        &crate::http::AttackReq,
        cast:          &crate::http::CastReq,
        sit:           &crate::http::SitReq,
        consider:      &crate::http::ConsiderReq,
        spells:        &crate::spells::SpellDb,
        spell_icons:   &[egui::TextureHandle],
        show_inventory: &mut bool,
        ui_zoom:       &mut f32,
        ui_zoom_size:  &mut (u32, u32),
        show_debug:    bool,
        corrections:   u32,
    ) {
        let (Some(egui_state), Some(egui_renderer), Some(egui_ctx), Some(window)) =
            (egui_state, egui_renderer, egui_ctx, window) else { return };

        let raw_input = egui_state.take_egui_input(window);
        let view_proj = renderer.last_view_proj;
        let screen_w  = renderer.surface_config.width;
        let screen_h  = renderer.surface_config.height;

        // Scale the entire UI (text + widgets) to a fixed 1920x1080 design layout by the CONSTRAINING
        // window dimension: zoom = min(w/1920, h/1080) (the smaller ratio fits without overflow), so
        // a 16:9 window matches 1:1 and other aspect ratios scale uniformly. Divided by the native
        // DPI ppp so it's display-independent. Cached; only recomputed when the surface size changes.
        if (screen_w, screen_h) != *ui_zoom_size {
            let nppp = window.scale_factor() as f32;
            let (rw, rh) = (hud::HUD_REF_W, hud::HUD_REF_H);
            *ui_zoom = ((screen_w as f32 / rw).min(screen_h as f32 / rh) / nppp).max(0.05);
            *ui_zoom_size = (screen_w, screen_h);
        }
        egui_ctx.set_zoom_factor(*ui_zoom);

        let full_output = egui_ctx.run(raw_input, |ctx| {
            hud::draw_fps(ctx, current_fps);
            if loading {
                hud::draw_loading(ctx, current_zone, load_status);
            } else {
                hud::draw_ui_menu(ctx, ui_layout);
                hud::draw_hud(ctx, ui_layout, scene, "EQ Observer");
                hud::draw_quest_dialogue(ctx, ui_layout, scene, say);
                hud::draw_message_log(ctx, ui_layout, scene);
                hud::draw_labels(ctx, scene, view_proj, screen_w, screen_h, cam_eye, collision);
                hud::draw_minimap(ctx, ui_layout, scene, zone_min, zone_max, minimap_zoom, minimap_full, zone_map);
                hud::draw_control_bar(ctx, ui_layout, scene, hail, say, target, say_buffer);
                hud::draw_action_grid(ctx, ui_layout, scene, spells, spell_icons, attack, cast, sit, target, consider);
                hud::draw_inventory(ctx, ui_layout, scene, show_inventory);
                if show_debug {
                    hud::draw_debug_overlay(ctx, scene.player_pos, scene.player_heading, current_zone, corrections);
                }
            }
        });
        ui_layout.end_frame();
        ui_layout.maybe_save();
        egui_state.handle_platform_output(window, full_output.platform_output);
        // egui auto-enables IME when a text field is focused; on Linux that hands keystrokes
        // to the system IME (fcitx/ibus) which composes instead of delivering them, so the
        // chat box never receives text. Force IME off so keys arrive as normal KeyEvent.text.
        window.set_ime_allowed(false);

        let primitives  = egui_ctx.tessellate(full_output.shapes, full_output.pixels_per_point);
        let screen_desc = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [screen_w, screen_h],
            pixels_per_point: full_output.pixels_per_point,
        };
        for (id, delta) in &full_output.textures_delta.set {
            egui_renderer.update_texture(&renderer.device, &renderer.queue, *id, delta);
        }
        egui_renderer.update_buffers(&renderer.device, &renderer.queue, encoder, &primitives, &screen_desc);
        {
            let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("egui"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None, occlusion_query_set: None,
            });
            egui_renderer.render(&mut pass.forget_lifetime(), &primitives, &screen_desc);
        }
        for id in &full_output.textures_delta.free { egui_renderer.free_texture(id); }
    }

    /// Submit the command buffer; if a /frame capture is pending, copy the
    /// texture to a staging buffer first and encode it as PNG.
    fn submit_frame(
        frame_req: &FrameReq,
        encoder:   wgpu::CommandEncoder,
        output:    wgpu::SurfaceTexture,
        renderer:  &EqRenderer,
    ) {
        let pending_tx = frame_req.lock().unwrap().take();
        if let Some(tx) = pending_tx {
            let w         = renderer.surface_config.width;
            let h         = renderer.surface_config.height;
            let row_pitch = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
                * ((w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
            let staging = renderer.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("frame_staging"), size: (row_pitch * h) as u64,
                usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let mut enc = encoder;
            enc.copy_texture_to_buffer(
                output.texture.as_image_copy(),
                wgpu::ImageCopyBuffer {
                    buffer: &staging,
                    layout: wgpu::ImageDataLayout {
                        offset: 0, bytes_per_row: Some(row_pitch), rows_per_image: None,
                    },
                },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
            renderer.queue.submit(std::iter::once(enc.finish()));
            output.present();
            renderer.device.poll(wgpu::Maintain::Wait);
            let slice = staging.slice(..);
            slice.map_async(wgpu::MapMode::Read, |_| {});
            renderer.device.poll(wgpu::Maintain::Wait);
            let png = encode_frame_png(
                &slice.get_mapped_range(), w, h, row_pitch, renderer.surface_config.format,
                Some(512),
            );
            let _ = tx.send(png);
        } else {
            renderer.queue.submit(std::iter::once(encoder.finish()));
            output.present();
        }
    }
}

// ── winit event handler ───────────────────────────────────────────────────────

use std::mem;

impl ApplicationHandler for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        let window = Arc::new(
            event_loop
                .create_window(WindowAttributes::default().with_title("EQ Observer"))
                .expect("create window"),
        );
        self.init_gpu(window);
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id:        winit::window::WindowId,
        event:      WindowEvent,
    ) {
        if let (Some(egui_state), Some(window)) = (&mut self.egui_state, &self.window) {
            if egui_state.on_window_event(window, &event).consumed { return; }
        }

        match event {
            WindowEvent::CloseRequested => { self.ui_layout.save_now(); event_loop.exit(); }

            WindowEvent::Resized(size) => {
                if let Some((surface, renderer)) = &mut self.gpu {
                    renderer.surface_config.width  = size.width.max(1);
                    renderer.surface_config.height = size.height.max(1);
                    surface.configure(&renderer.device, &renderer.surface_config);
                    renderer.recreate_depth_texture();
                }
            }

            WindowEvent::RedrawRequested => {
                self.render_frame();
                // Defer zone reload until after GPU borrow is fully released.
                if mem::take(&mut self.pending_reload) {
                    self.reload_zone();
                }
                // Check whether the background load thread finished; do GPU upload if so.
                self.maybe_finish_load();
            }

            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                match state {
                    ElementState::Pressed => {
                        self.drag_active = true;
                        self.click_start = Some(self.last_cursor);
                    }
                    ElementState::Released => {
                        self.drag_active = false;
                        if let Some(start) = self.click_start.take() {
                            let dx = (self.last_cursor.x - start.x) as f32;
                            let dy = (self.last_cursor.y - start.y) as f32;
                            // Less than 5-pixel movement → treat as a click, not drag
                            if dx * dx + dy * dy < 25.0 {
                                if let Some(id) = self.pick_at(self.last_cursor) {
                                    self.game_state.target_id   = Some(id);
                                    self.game_state.target_con  = None;
                                    if let Some(e) = self.game_state.entities.get(&id) {
                                        self.game_state.target_name   = Some(e.name.clone());
                                        self.game_state.target_hp_pct = Some(e.hp_pct);
                                    }
                                    *self.target.lock().unwrap() = Some(id);
                                }
                            }
                        }
                    }
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.drag_active {
                    let dx = (position.x - self.last_cursor.x) as f32;
                    let dy = (position.y - self.last_cursor.y) as f32;
                    self.camera.apply_orbit_delta(dx * 0.005, dy * 0.005);
                }
                self.last_cursor = position;
            }

            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    MouseScrollDelta::PixelDelta(p)   => p.y as f32 * 0.002,
                };
                if lines.abs() > 1e-6 { self.camera.apply_zoom(lines * 0.1); }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if let PhysicalKey::Code(code) = event.physical_key {
                    match event.state {
                        ElementState::Pressed => {
                            match code {
                                KeyCode::KeyW | KeyCode::KeyA | KeyCode::KeyS | KeyCode::KeyD
                                | KeyCode::KeyQ | KeyCode::KeyE | KeyCode::Space
                                | KeyCode::ControlLeft | KeyCode::ControlRight => {
                                    self.keys_held.insert(code);
                                }
                                KeyCode::KeyR | KeyCode::F9 => {
                                    self.camera.reset_to_follow();
                                    self.override_pos = None;
                                    *self.goto_target.lock().unwrap() = None;
                                }
                                KeyCode::F10 => {
                                    self.show_debug = !self.show_debug;
                                    eprintln!("DEBUG: overlay {}", if self.show_debug { "ON" } else { "OFF" });
                                }
                                KeyCode::KeyI => {
                                    self.show_inventory = !self.show_inventory;
                                }
                                KeyCode::KeyL
                                    if self.keys_held.contains(&KeyCode::ControlLeft)
                                        || self.keys_held.contains(&KeyCode::ControlRight) =>
                                {
                                    self.ui_layout.locked = !self.ui_layout.locked;
                                    self.ui_layout.set_dirty_locked();
                                }
                                _ => {}
                            }
                        }
                        ElementState::Released => {
                            self.keys_held.remove(&code);
                        }
                    }
                }
            }

            _ => {}
        }
    }
}
