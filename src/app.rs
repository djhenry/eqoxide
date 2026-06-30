//! Application window, render loop, and input handling.

use std::sync::{Arc, Mutex};
use winit::{
    application::ApplicationHandler,
    event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, ControlFlow},
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

/// Result of a left-click pick test: the nearest entity or door the ray hit, if any.
#[derive(Clone, Copy)]
pub enum PickResult {
    Entity(u32),
    Door(u8),
}

/// The winit `ApplicationHandler` and root of the render half. Owns the window + GPU surface, the
/// Per-entity motion smoothing state. Server position updates (OP_ClientUpdate) arrive only
/// a few times per second; we estimate each entity's velocity from the last two updates and
/// dead-reckon its position forward so movement looks continuous and travels at the right pace,
/// instead of snapping or easing toward a stale point in bursts.
struct EntityMotion {
    /// Smoothed position actually rendered [east, north, z].
    display:     [f32; 3],
    /// Most recent server position seen [east, north, z].
    target:      [f32; 3],
    /// Estimated travel pace in units/sec, from the last two server positions. We move `display`
    /// toward `target` at this pace (never overshooting) so the entity glides between sparse
    /// updates at its actual speed instead of lurching to each one and waiting.
    speed:       f32,
    /// When `target` last changed — used to measure the real per-update interval.
    last_update: std::time::Instant,
}

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
    /// Set on every zone change. While true, the first frame with loaded collision settles the
    /// player onto the NEAREST floor (above or below), fixing zone-ins where the zone-point z is
    /// below the actual floor (the per-frame snap only probes downward and can't lift them).
    needs_reground:     bool,
    last_frame_time:    std::time::Instant,
    fps_frame_count:    u32,
    fps_timer:          std::time::Instant,
    current_fps:        f32,
    /// Event-driven scheduling: render at full rate until this instant, then drop to an idle poll.
    /// Bumped forward by `wake()` whenever something happens (input, packet, animation in flight).
    /// When `now >= active_until` and nothing is pending, the loop only wakes to poll the network
    /// channel — so a still scene costs ~no CPU. See `about_to_wait`.
    active_until:       std::time::Instant,
    /// Smoothed per-phase frame timings for the `--profile` HUD overlay (only written when enabled).
    frame_profile:      crate::profiling::FrameProfile,
    // Keyboard movement
    keys_held:    std::collections::HashSet<KeyCode>,
    /// Free-fly position override in scene space [east, north, z].
    /// None = track server position; Some = keyboard-driven position.
    override_pos: Option<[f32; 3]>,
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
    /// Merchant buy/sell/open-close request slots written by the HUD merchant window.
    buy:          crate::http::BuyReq,
    sell:         crate::http::SellReq,
    trade:        crate::http::TradeReq,
    spells:       std::sync::Arc<crate::spells::SpellDb>,
    /// Shared door-click request slot; the nav thread drains it and sends OP_ClickDoor.
    door_click:   crate::http::DoorClickReq,
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
    /// Set by every shutdown path (POST /exit, OP_GMKick). Observed in `about_to_wait` to exit the
    /// winit event loop on the MAIN thread, so winit tears down its Wayland clipboard worker cleanly
    /// — instead of a background thread calling `process::exit()` and racing that teardown (SIGSEGV).
    shutdown:     std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Camp command slot (HUD Camp button writes a Toggle here) and the published camp deadline the
    /// HUD reads for its countdown. The gameplay loop owns the camp logic; see `gameplay::camp_apply`.
    camp:         crate::http::CampReq,
    camp_until:   crate::http::CampUntil,
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
    /// Per-entity motion smoothing state, keyed by spawn id. See [`EntityMotion`].
    entity_motion: std::collections::HashMap<u32, EntityMotion>,
    /// Estimated nav-driven speed for the visual player position glide (units/s).
    /// Measured from consecutive logical position changes; defaults to RUN_SPEED.
    player_nav_speed: f32,
    /// When the logical player position last changed, for speed estimation.
    last_player_nav_update: std::time::Instant,
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
    /// Whether the map window is open (toggled by the HUD button or the M key). Defaults closed.
    show_map: bool,
    ui_layout: crate::ui_layout::UiLayout,
    /// Cached egui textures for spell-gem icons (spells01..07.tga). Empty until first render.
    spell_icons: Vec<egui::TextureHandle>,
    /// True once `load_spell_icons` has been attempted (avoids retrying every frame after failure).
    tried_icons: bool,
    /// Cached global UI zoom factor (min(w/1920, h/1080) / dpi) and the surface size it was computed
    /// for — recomputed only when the size changes, not every frame.
    ui_zoom: f32,
    ui_zoom_size: (u32, u32),
    /// Asset-sync progress fraction (0.0–1.0) shown on the loading screen; None when not syncing.
    sync_progress: std::sync::Arc<std::sync::Mutex<Option<f32>>>,
    /// Set to Some(Ok(())) when the common-model sync finishes, Some(Err(msg)) on failure.
    sync_done: std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>,
    /// True once character models have been loaded from the cache (guards one-time load).
    models_loaded: bool,
    asset_server_url: String,
    asset_user: String,
    asset_pass: String,
}

impl App {
    pub fn new(
        // Vestigial: everything now loads via models_path / the asset cache.
        // Kept for call-site stability (mirrors renderer::load_character_models).
        _assets_path:    std::path::PathBuf,
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
        buy:             crate::http::BuyReq,
        sell:            crate::http::SellReq,
        trade:           crate::http::TradeReq,
        spells:          std::sync::Arc<crate::spells::SpellDb>,
        door_click:      crate::http::DoorClickReq,
        shared_collision: assets::SharedCollision,
        player_info:     crate::http::PlayerInfo,
        warp:            crate::http::WarpReq,
        testzone_mode:   bool,
        shutdown:        std::sync::Arc<std::sync::atomic::AtomicBool>,
        camp:            crate::http::CampReq,
        camp_until:      crate::http::CampUntil,
        asset_server_url: String,
        asset_user:       String,
        asset_pass:       String,
    ) -> Self {
        let ui_layout = crate::ui_layout::UiLayout::load(&character_name);
        let mut game_state = GameState::new();
        game_state.player_name = character_name;

        if testzone_mode {
            game_state.zone_name = "testzone".to_string();
            game_state.zone_changed = true;
            tracing::info!("APP: --testzone mode, will load debug zone");
        }

        App {
            window: None, gpu: None, egui_ctx: None, egui_state: None, egui_renderer: None,
            models_path,
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
            needs_reground: false,
            last_frame_time: std::time::Instant::now(),
            fps_frame_count: 0,
            fps_timer: std::time::Instant::now(),
            current_fps: 0.0,
            active_until: std::time::Instant::now(),
            frame_profile: crate::profiling::FrameProfile::default(),
            keys_held: std::collections::HashSet::new(),             override_pos: None, goto_target,
            hail, say, target, attack, cast, sit, consider, buy, sell, trade, spells, door_click, say_buffer: String::new(),
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
            player_info, warp, shutdown, camp, camp_until, collision: None, shared_collision,
            ground_cache: (f32::NAN, f32::NAN, 0.0),
            last_grounded_z: 0.0,
            prev_render_pos: [0.0, 0.0, 0.0],
            entity_motion: std::collections::HashMap::new(),
            player_nav_speed: 44.0, // default to RUN_SPEED until first measurement
            last_player_nav_update: std::time::Instant::now(),
            heading_target:  0.0,
            visual_heading:  0.0,
            vert_vel:  0.0,
            on_ground: true,
            testzone_mode,
            show_debug: false,
            show_inventory: false,
            show_map: false,
            ui_layout,
            spell_icons: Vec::new(),
            tried_icons: false,
            ui_zoom: 1.0,
            ui_zoom_size: (0, 0),
            sync_progress: Arc::new(Mutex::new(None)),
            sync_done:     Arc::new(Mutex::new(None)),
            models_loaded: false,
            asset_server_url, asset_user, asset_pass,
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
    fn pick_at(&self, cursor: winit::dpi::PhysicalPosition<f64>) -> Option<PickResult> {
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
        let mut best_t = f32::MAX;
        let mut best: Option<PickResult> = None;

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
                best_t = t;
                best   = Some(PickResult::Entity(id));
            }
        }

        // Doors: test against the door's real, oriented bounding box so the click target matches
        // the rendered door (the old 3-unit sphere was far smaller than most doors). Bounds come
        // from the loaded door model (render-space local AABB); missing models use a small default
        // cube matching the fallback box. The box is placed exactly like encode_door_pass:
        // T(pos) * Rz(yaw) * S(size/100). Incline is ignored for picking (negligible).
        let door_bounds = self.gpu.as_ref().map(|(_, r)| &r.door_bounds);
        const DEFAULT_DOOR_AABB: ([f32; 3], [f32; 3]) = ([-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        for d in self.game_state.doors.values() {
            let (bmin, bmax) = door_bounds
                .and_then(|b| b.get(&d.name.to_uppercase()))
                .copied()
                .unwrap_or(DEFAULT_DOOR_AABB);
            let scale = (d.size as f32 / 100.0).max(1e-3);
            let yaw   = (d.heading / 512.0) * std::f32::consts::TAU + std::f32::consts::FRAC_PI_2;
            let placement = glam::Mat4::from_translation(glam::Vec3::new(d.x, d.y, d.z))
                * glam::Mat4::from_rotation_z(yaw)
                * glam::Mat4::from_scale(glam::Vec3::splat(scale));
            let inv = placement.inverse();
            let lo  = inv.transform_point3(ray_origin);
            let ld  = inv.transform_vector3(ray_dir);
            if let Some(t_local) = crate::camera::ray_aabb(lo.to_array(), ld.to_array(), bmin, bmax) {
                // Convert the local-space hit back to a world-space distance for fair comparison
                // with the entity hits above (local `dir` is unnormalised by the inverse scale).
                let world_hit = placement.transform_point3(lo + ld * t_local);
                let t_world = (world_hit - ray_origin).dot(ray_dir);
                if t_world > 0.0 && t_world < best_t {
                    best_t = t_world;
                    best   = Some(PickResult::Door(d.door_id));
                }
            }
        }

        best
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
                tracing::info!("renderer: debug zone loaded ({} meshes)", renderer.gpu_meshes.len());
            }
            self.loading = false;
            return;
        }

        // Zone maps (minimap) + water regions come from the asset server's "gamedata" set in the
        // local cache (synced at startup), not from ~/eq_assets.
        let maps_dir    = crate::asset_sync::CacheDirs::resolve().models_dir().join("maps");
        let load_status = self.load_status.clone();
        let pending     = self.pending_load.clone();
        let url  = self.asset_server_url.clone();
        let user = self.asset_user.clone();
        let pass = self.asset_pass.clone();

        *load_status.lock().unwrap() = "Connecting to asset server…".to_string();

        std::thread::spawn(move || {
            let set_status = |s: &str| { *load_status.lock().unwrap() = s.to_string(); };

            let cache = crate::asset_sync::CacheDirs::resolve();
            set_status("Connecting to asset server…");
            let loaded = (|| -> anyhow::Result<assets::ZoneAssets> {
                let sync = crate::asset_sync::AssetSync::login(&url, &user, &pass)?;
                set_status("Verifying zone assets…");
                let dl_status = load_status.clone();
                crate::asset_sync::sync_set(&sync, &format!("zone/{zone_name}"), &cache, &mut |p| {
                    if matches!(p.phase, crate::asset_sync::Phase::Downloading) {
                        let mb = p.bytes as f64 / 1_048_576.0;
                        *dl_status.lock().unwrap() =
                            format!("Downloading zone {}/{} ({:.1} MB)…", p.done, p.total, mb);
                    }
                })?;
                // Door/object models for clickable doors come from the asset server's
                // "zonedoors/<zone>" set (the raw <zone>_obj.s3d) into the cache — never ~/eq_assets.
                // Best-effort: if it's absent, load_door_models falls back to plain boxes.
                let _ = crate::asset_sync::sync_set(&sync, &format!("zonedoors/{zone_name}"), &cache, &mut |_| {});
                set_status("Reading zone geometry…");
                assets::ZoneAssets::from_glb(&cache.models_dir().join(format!("{zone_name}.glb")))
            })();
            let (opt_assets, zone_min, zone_max) = match loaded {
                Ok(za) => {
                    let (mn, mx) = za.bounds_xy().unwrap_or(([0.0f32;2],[0.0f32;2]));
                    (Some(za), mn, mx)
                }
                Err(e) => { tracing::warn!("renderer: zone '{}' load failed: {}", zone_name, e); (None, [0.0f32;2],[0.0f32;2]) }
            };

            set_status("Building collision grid…");
            // Load the zone's water regions (maps/water/<zone>.wtr) so find_path can swim/descend
            // through water where there's no walkable connection. None if the zone has no .wtr.
            let water = crate::water_map::WaterMap::load(&maps_dir.join("water"), &zone_name).map(Arc::new);
            let collision = opt_assets.as_ref().map(|za| {
                let mut c = assets::Collision::build(za, 32.0);
                c.set_water(water);
                Arc::new(c)
            });

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

        // Path for this zone's door/object models — from the asset-server cache ("zonedoors/<zone>"
        // set), as a pre-baked GLB. Best-effort: if absent, load_door_models falls back to boxes.
        let cache_models = crate::asset_sync::CacheDirs::resolve().models_dir();
        let door_glb = cache_models.join(format!("{}_doors.glb", load.zone_name));

        if let Some((_, renderer)) = &mut self.gpu {
            match load.assets {
                Some(ref za) => {
                    renderer.upload_zone_assets(za);
                    tracing::info!("renderer: uploaded {} meshes for '{}'", renderer.gpu_meshes.len(), load.zone_name);
                    // Load this zone's door/object models for clickable-door rendering.
                    renderer.load_door_models(&door_glb);
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

    /// Drains the asset-sync result on the main thread and loads character models
    /// from the cache once the sync thread signals done.
    fn poll_sync(&mut self) {
        if self.models_loaded { return; }
        let done = self.sync_done.lock().unwrap().take();
        if let Some(result) = done {
            match result {
                Ok(()) => {
                    if let Some((_, renderer)) = &mut self.gpu {
                        // Both args are the cache now (equip/weapon S3Ds come from the "gameequip"
                        // set in the cache); the 2nd arg is ignored but kept for signature stability.
                        renderer.load_character_models(&self.models_path, &self.models_path);
                    }
                    self.models_loaded = true;
                    self.loading = false;
                    *self.sync_progress.lock().unwrap() = None;
                }
                Err(msg) => {
                    *self.load_status.lock().unwrap() = msg;
                    // stay on the loading screen showing the error; do not load blob fallback.
                }
            }
        }
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
        let renderer  = EqRenderer::new(device, queue, surface_config);
        // Resolve models to the cwd-independent XDG cache and sync the `common`
        // set from the asset server before loading character models.
        let cache = crate::asset_sync::CacheDirs::resolve();
        self.models_path = cache.models_dir();
        self.loading = true;
        *self.load_status.lock().unwrap() = "Connecting to asset server…".to_string();

        let url = self.asset_server_url.clone();
        let user = self.asset_user.clone();
        let pass = self.asset_pass.clone();
        let status = self.load_status.clone();
        let progress = self.sync_progress.clone();
        let done = self.sync_done.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let sync = crate::asset_sync::AssetSync::login(&url, &user, &pass)?;
                *status.lock().unwrap() = "Verifying assets…".to_string();
                crate::asset_sync::sync_set(&sync, "common", &cache, &mut |p| {
                    match p.phase {
                        crate::asset_sync::Phase::Verifying => {
                            *status.lock().unwrap() = "Verifying assets…".to_string();
                            *progress.lock().unwrap() = None;
                        }
                        crate::asset_sync::Phase::Downloading => {
                            let mb = p.bytes as f64 / 1_048_576.0;
                            *status.lock().unwrap() =
                                format!("Downloading {}/{} ({:.1} MB)…", p.done, p.total, mb);
                            let frac = if p.total > 0 { p.done as f32 / p.total as f32 } else { 1.0 };
                            *progress.lock().unwrap() = Some(frac);
                        }
                    }
                })?;
                Ok(())
            })();

            // Fail loud unless the cache already satisfies us: if reassembled models
            // exist, proceed; otherwise surface the error.
            let satisfied = cache.models_dir().exists()
                && std::fs::read_dir(cache.models_dir())
                    .map(|mut d| d.any(|e| e.map(|e| e.path().extension().map_or(false, |x| x == "glb")).unwrap_or(false)))
                    .unwrap_or(false);
            let final_result = match result {
                Ok(()) => Ok(()),
                Err(e) if satisfied => {
                    *status.lock().unwrap() =
                        format!("Asset server unavailable ({e}); using cached models.");
                    Ok(())
                }
                Err(e) => Err(format!("Asset sync failed and no cached models: {e}")),
            };
            *done.lock().unwrap() = Some(final_result);
        });
        self.egui_ctx      = Some(egui_ctx);
        self.egui_state    = Some(egui_state);
        self.egui_renderer = Some(egui_renderer);
        self.gpu           = Some((surface, renderer));
        self.window        = Some(window);
    }

    // ── Render loop ───────────────────────────────────────────────────────────

    /// How long after the last activity to keep rendering at full rate before dropping to idle poll.
    /// Covers animation tails (door swing, position glide, camera ease) and keeps input feeling crisp.
    const ACTIVE_LINGER: std::time::Duration = std::time::Duration::from_millis(300);
    /// Frame interval while active (~60 fps).
    const FRAME_INTERVAL: std::time::Duration = std::time::Duration::from_millis(16);
    /// Idle wake cadence — just often enough to drain the network channel promptly without burning
    /// CPU. A still scene wakes ~20×/sec, does a `try_recv` on an empty channel, and sleeps again.
    const IDLE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

    /// Mark the app active (render at full rate for `ACTIVE_LINGER`) and request a redraw now. Called
    /// from input handlers and whenever `poll_external` finds pending work.
    fn wake(&mut self) {
        self.active_until = std::time::Instant::now() + Self::ACTIVE_LINGER;
        if let Some(w) = &self.window { w.request_redraw(); }
    }

    /// Drain the EQ packet channel into game state and report whether anything warrants rendering.
    /// Runs every `about_to_wait` (even idle ones) so the network keeps flowing without a render.
    /// Returns true when visible state is changing or pending: queued packets, an active zone load,
    /// player input/motion in flight, easing doors/position/heading, or a queued HTTP request that a
    /// render must service (frame capture / camera / warp).
    fn poll_external(&mut self) -> bool {
        let mut activity = false;

        // Drain all queued packets; any packet may move/spawn an entity, so treat as activity.
        while let Ok(packet) = self.app_rx.try_recv() {
            apply_packet(&mut self.game_state, &packet);
            activity = true;
        }

        // Still loading a zone, or a reload is queued → keep rendering the progress screen.
        if self.loading || self.pending_reload { activity = true; }

        // A queued HTTP request that only a render frame can service.
        if self.frame_req.lock().is_ok_and(|g| g.is_some()) { activity = true; }
        if self.camera_cmd.lock().is_ok_and(|g| g.is_some()) { activity = true; }
        if self.warp.lock().is_ok_and(|g| g.is_some()) { activity = true; }

        // Player input / motion in flight (keys held, free-fly override active, or falling).
        if !self.keys_held.is_empty() || self.override_pos.is_some() || !self.on_ground {
            activity = true;
        }

        // Doors still easing toward their open/closed target.
        if self.game_state.doors.values()
            .any(|d| (d.open_frac - if d.is_open { 1.0 } else { 0.0 }).abs() > 0.001)
        {
            activity = true;
        }

        // Visual position still gliding toward the logical (server-authoritative) position.
        let dx = self.game_state.player_x - self.visual_player_pos[0];
        let dy = self.game_state.player_y - self.visual_player_pos[1];
        if dx * dx + dy * dy > 0.01 { activity = true; }

        // Heading still smoothing toward its target.
        let hd = (self.heading_target - self.visual_heading).rem_euclid(360.0);
        if hd > 0.05 && hd < 359.95 { activity = true; }

        // Character animations (idle/walk/etc.) loop continuously. Keep rendering while any is in
        // flight so they actually PLAY, instead of freezing on a single frame whenever the scene is
        // otherwise still (no packets/input) — which made standing characters look frozen in a
        // static pose. `animate` is false for held poses (sitting, dead, idle-on-a-walk-fallback),
        // so a truly motionless scene still drops to the idle poll.
        if self.gpu.as_ref().is_some_and(|(_, r)| r.anim_states.values().any(|s| s.animate)) {
            activity = true;
        }

        activity
    }

    fn render_frame(&mut self) {
        // Compute dt at the very top so it's available for animation before SceneState is built.
        let now = std::time::Instant::now();
        let dt  = (now - self.last_frame_time).as_secs_f32().min(0.1);
        self.last_frame_time = now;

        // Wall-clock since the previous rendered frame, for the profile overlay's "frame" / fps line.
        // (`dt` above is clamped to 0.1; this is the unclamped real interval, which during idle waits
        // can legitimately be long.)
        let frame_ms = dt * 1000.0;
        let prof_update = crate::profiling::Stopwatch::start();

        // EQ packets are drained in `poll_external` (called from `about_to_wait` every wake) so the
        // network keeps flowing even on idle frames that don't render. `game_state` is already current
        // here.

        // Warp (POST /warp) is handled authoritatively by the NAV thread (see navigation.rs),
        // which teleports the server-side position AND cancels any in-progress /goto. We only
        // PEEK it here for instant local visual feedback (clearing any WASD override so the
        // render follows the new server position); the nav thread is the slot's sole consumer.
        // The old code instead wrote the warp coords into goto_target, which made the nav thread
        // try to *walk* there and stall — a warp could then be dragged back to a stuck path.
        let warp_peek = *self.warp.lock().unwrap();
        if let Some((wx, wy, wz)) = warp_peek {
            self.game_state.player_x = wx;
            self.game_state.player_y = wy;
            self.game_state.player_z = wz;
            self.visual_player_pos = [wx, wy, wz];
            self.override_pos = None;
        }

        // Ease each door's render fraction toward its server-authoritative open/close target.
        {
            let step = (dt / 0.5).min(1.0); // ~0.5s full travel
            for d in self.game_state.doors.values_mut() {
                let target = if d.is_open { 1.0_f32 } else { 0.0_f32 };
                d.open_frac += (target - d.open_frac) * step;
                if (d.open_frac - target).abs() < 0.001 { d.open_frac = target; }
            }
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
                coin:       gs.coin,
                hp_pct:        gs.hp_pct,
                cur_hp:        gs.cur_hp,
                max_hp:        gs.max_hp,
                mana_pct:      gs.mana_pct,
                xp_pct:        gs.xp_pct,
                // Prefer the live entity (its hp_pct tracks combat via OP_HP_UPDATE); fall back to
                // the target snapshot stored at target time if the entity is gone.
                target_name:   gs.target_id.and_then(|id| gs.entities.get(&id)).map(|e| e.name.clone())
                                   .or_else(|| gs.target_name.clone()),
                target_hp_pct: gs.target_id.and_then(|id| gs.entities.get(&id)).map(|e| e.hp_pct)
                                   .or(gs.target_hp_pct),
            };
        }

        // In the test zone, inject fake billboards so every loaded character model
        // is rendered side-by-side for visual debugging.
        if self.scene.zone == "testzone" {
            self.scene.inject_test_billboards();
        }

        // Smooth NPC movement. Server position updates (OP_ClientUpdate) arrive only a few
        // times per second, so snapping each billboard to the latest packet looks choppy.
        // Instead we estimate each entity's velocity from its last two server positions and
        // dead-reckon it forward, so it travels continuously at its actual pace. Large
        // horizontal jumps (spawns, teleports, server corrections) snap instead of sliding.
        // Done before the floor-snap below so the ground height follows the smoothed position.
        {
            const SNAP_DIST_SQ: f32 = 25.0 * 25.0; // beyond this horizontal gap, jump not slide
            const MAX_UPD: f32 = 4.0;              // cap on the measured update interval. RoF2 NPCs
                                                   // send a position only ~every 2.7s; the old 1.0s
                                                   // cap made the pace estimate ~3x too high, so the
                                                   // entity lurched to each point then waited.
            let now = std::time::Instant::now();
            let live: std::collections::HashSet<u32> =
                self.scene.billboards.iter().map(|b| b.id).collect();
            self.entity_motion.retain(|id, _| live.contains(id));
            for b in &mut self.scene.billboards {
                let target = b.pos;
                let m = self.entity_motion.entry(b.id).or_insert_with(|| EntityMotion {
                    display: target, target, speed: 0.0, last_update: now,
                });

                // A changed server position is a fresh update: estimate the travel pace from the
                // distance moved since the previous one over the real elapsed interval.
                if target != m.target {
                    let dx = target[0] - m.target[0];
                    let dy = target[1] - m.target[1];
                    let dz = target[2] - m.target[2];
                    if dx * dx + dy * dy >= SNAP_DIST_SQ {
                        m.speed = 0.0;          // teleport / correction — snap, don't slide across
                        m.display = target;
                    } else {
                        let dt_upd = (now - m.last_update).as_secs_f32().clamp(0.05, MAX_UPD);
                        m.speed = (dx * dx + dy * dy + dz * dz).sqrt() / dt_upd;
                    }
                    m.target = target;
                    m.last_update = now;
                }

                // Glide the rendered position toward the latest server position at that pace, never
                // overshooting: a moving entity travels smoothly over the whole update gap and a
                // stopped one settles cleanly (no extrapolation drift past its last point).
                let to = [target[0] - m.display[0], target[1] - m.display[1], target[2] - m.display[2]];
                let d = (to[0] * to[0] + to[1] * to[1] + to[2] * to[2]).sqrt();
                if d > 1e-4 {
                    let move_d = (m.speed * dt).min(d);
                    let f = move_d / d;
                    for i in 0..3 { m.display[i] += to[i] * f; }
                }
                b.pos = m.display;

                // Override "idle" action with "walking" when the entity is actively moving
                // toward its server target. Preserves dead / combat / sitting overrides —
                // only replaces "idle" (the default for all non-dead, non-swinging entities
                // from scene.rs, since the server animation field is always "Standing" while
                // an NPC moves between update packets).
                if b.action == "idle" && m.speed > 0.5 && d > 1e-4 {
                    b.action = "walking".to_string();
                }
            }
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
            let nav_dist = (dx * dx + dy * dy).sqrt();
            if nav_dist > 0.01 {
                // Estimate nav-driven speed from the distance moved over the elapsed interval.
                // Clamped to [50ms, 500ms] so a stale first frame doesn't spike the estimate.
                let dt_upd = (now - self.last_player_nav_update).as_secs_f32().clamp(0.05, 0.5);
                self.player_nav_speed = nav_dist / dt_upd;
                self.last_player_nav_update = now;
                self.last_moved_at = std::time::Instant::now();
            }
            self.prev_logical_pos = lp;
            // Priority: dead > combat swing > walking > idle.
            let pid = self.game_state.player_id;
            let player_dead = self.game_state.cur_hp <= 0 && self.game_state.max_hp > 0;
            let swinging = self.game_state.combat_anims.get(&pid)
                .map_or(false, |(_, t)| t.elapsed() < crate::scene::COMBAT_SWING_WINDOW);
            self.scene.player_action = if player_dead {
                "dead".to_string()
            } else if let Some((code, _)) = self.game_state.combat_anims.get(&pid).filter(|_| swinging) {
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

        // Trigger a zone (re)load whenever the zone we're standing in differs from the zone whose
        // geometry is currently loaded. We deliberately do NOT gate on the transient
        // `scene.zone_changed` edge flag: OP_NewZone sets it and OP_Weather clears it, and both
        // packets often arrive in the same `poll_external` drain — so the true→false transition can
        // happen entirely between two scene snapshots and never be observed here, leaving the player
        // in a terrain-less void (since `current_zone` then never advances). Comparing against the
        // durable `current_zone` (what we've actually loaded) is a level condition that can't be
        // missed by drain timing. See `zone_needs_reload`.
        if zone_needs_reload(&self.scene.zone, &self.current_zone) {
            self.loading       = true;
            self.pending_reload = true;
            self.current_zone  = self.scene.zone.clone();
            // Drop the OLD zone's collision immediately so nothing grounds against or collides with
            // stale geometry while the new zone loads (the player is already at new-zone coords).
            // The new collision is swapped in atomically when the load completes.
            self.collision = None;
            *self.shared_collision.write().unwrap() = None;
            // The new zone's floor may sit above the zone-point spawn z; settle onto it once
            // collision loads (see the reground block in the vertical-physics section below).
            self.needs_reground = true;
        }

        // Fresh `now` for the FPS timer; `dt` and `last_frame_time` were already updated at top.
        let now = std::time::Instant::now();

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
        let w_held = self.keys_held.contains(&KeyCode::KeyW);
        let s_held = self.keys_held.contains(&KeyCode::KeyS);
        let q_held = self.keys_held.contains(&KeyCode::KeyQ);
        let e_held = self.keys_held.contains(&KeyCode::KeyE);
        // Rotate mode: LMB is up (not dragging camera). Strafe mode: LMB held.
        let rotating = !self.drag_active && (a_held || d_held);
        // Any manual movement key held. When true, the player's facing is driven by heading_target
        // (a/d rotation or mouse-look), NOT by motion direction — so strafing keeps facing forward
        // instead of turning to face the sideways motion. Motion-derived heading is only for /goto.
        let manual_move = a_held || d_held || w_held || s_held || q_held || e_held;
        // Mouse-look "drive": LMB held AND a movement key held -> the character's heading is slaved
        // to the camera each frame (steer with the mouse). With LMB held but no move key, the mouse
        // just orbits the camera (handled in input) and the heading is left alone.
        let lmb_drive = self.drag_active && manual_move;
        // Swim (vertical movement) only while driving forward/back AND standing in a water region.
        let in_water = self.collision.as_ref().is_some_and(|c| c.in_water(self.scene.player_pos));
        let swimming = lmb_drive && in_water && (w_held || s_held);

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
                let mut dh = 0.0;
                if a_held { dh -= TURN_SPEED * dt; }
                if d_held { dh += TURN_SPEED * dt; }
                self.heading_target = (self.heading_target + dh).rem_euclid(360.0);
                // Rotate the camera rigidly WITH the heading by the same delta, preserving its
                // current relative offset (it does NOT snap behind). Only F9/R resets to behind.
                self.camera.rotate_with_heading(dh.to_radians());
            }

            // Forward basis. In mouse-look drive mode the heading is slaved to the camera and W/S
            // move along the camera direction (with a vertical term when swimming). Otherwise W/S
            // move along the character's own heading. Strafe is always perpendicular to the heading.
            let (fwd_e, fwd_n, fwd_z) = if lmb_drive {
                let az = self.camera.azimuth;
                self.heading_target = crate::camera_state::heading_deg_from_azimuth(az);
                let d = crate::camera::camera_move_dir(az, self.camera.elevation, swimming);
                (d[0], d[1], d[2])
            } else {
                let h = self.heading_target.to_radians();
                // EQ heading: 0=north(+Y), increases CCW (90=west). Forward = (-sin h, cos h).
                (-h.sin(), h.cos(), 0.0)
            };
            // Right (strafe) vector: forward rotated -90° around the heading, always horizontal.
            let h = self.heading_target.to_radians();
            let (right_e, right_n) = (h.cos(), h.sin());

            let mut de = 0.0_f32;
            let mut dn = 0.0_f32;
            let mut dz = 0.0_f32;
            if w_held { de += fwd_e; dn += fwd_n; dz += fwd_z; }
            if s_held { de -= fwd_e; dn -= fwd_n; dz -= fwd_z; }
            // Strafe: Q = left, E = right (always); A/D strafe only while LMB (camera-orbit) is held.
            // Under the X-mirrored render, screen-left strafe moves along +right_vec and screen-right
            // along -right_vec — the same left/right reversal as the rotation fix above.
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

            if de != 0.0 || dn != 0.0 || dz != 0.0 {
                // Normalise over 3D so a diagonal swim isn't faster than a flat walk.
                let len = (de * de + dn * dn + dz * dz).sqrt();
                de = de / len * MOVE_SPEED * dt;
                dn = dn / len * MOVE_SPEED * dt;
                dz = dz / len * MOVE_SPEED * dt;
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
                    tracing::info!("COLLISION: WASD fully blocked at ({:.1},{:.1}) heading {:.0}° tried ({:.2},{:.2})",
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
                let new_z = if swimming {
                    // Swimming: move freely in Z by the camera pitch; bypass terrain step-up and
                    // gravity (the gravity block below is gated on !swimming).
                    self.on_ground = false;
                    base[2] + dz
                } else if self.on_ground
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
                // Speed-based glide: move the visual position toward the logical position at the
                // estimated nav pace, clamped so it never overshoots. This makes /goto travel
                // continuous — at RUN_SPEED (~44 u/s) the visual exactly keeps up with the
                // 6.6-unit nav steps every 150 ms, with no stutter between steps.
                let move_d = (self.player_nav_speed * dt).min(xy_dist);
                let f = move_d / xy_dist;
                self.visual_player_pos[0] += (target[0] - self.visual_player_pos[0]) * f;
                self.visual_player_pos[1] += (target[1] - self.visual_player_pos[1]) * f;
                // Z not lerped — ground snap owns it.
            }
            self.scene.player_pos = self.visual_player_pos;
        }

        // Vertical physics: fall under gravity, land on geometry, jump on spacebar.
        // Replaces the old static ground-snap. The floor query uses the player's current z
        // as anchor so balconies and ceilings above never read as the floor.
        // Skipped while swimming, which owns Z directly (camera-pitch driven).
        if !swimming {
            // Native Titanium fall: internal z-velocity clamps at ±128 EQ/s, and the outgoing
            // position packet's delta_z caps at 12.8/update (~10 Hz → ~128 EQ/s terminal). The old
            // 50/20 guess fell far too slowly. (eq-client-expert; docs/.../falling-physics.md.)
            const GRAVITY: f32       = 120.0; // EQ units/s² (reaches ~128 terminal in ~1s)
            const MAX_FALL: f32      = 128.0; // EQ units/s terminal velocity (native internal clamp)

            // One-shot reground after a zone change: the zone-point spawn z can be well BELOW the
            // zone's floor, and the downward-only ground-snap below can't lift the player (it would
            // leave them buried). ONLY when the player is actually below the floor (no floor found
            // beneath them) do we lift to the nearest floor above; a normal spawn at/above a floor
            // is left to the regular gravity/snap. Gated on !loading so it runs against the NEW
            // zone's collision (swapped in atomically with loading=false), never the old zone's.
            if self.needs_reground && !self.loading {
                if let Some(c) = self.collision.as_deref() {
                    let pp = self.scene.player_pos;
                    // floor_z returns the fallback (== pp[2]) when no surface is found below.
                    let no_floor_below = (c.floor_z(pp[0], pp[1], pp[2]) - pp[2]).abs() < 0.01;
                    if no_floor_below {
                        // Buried: find the floor above (generous up band) and lift onto it.
                        const REGROUND_UP: f32 = 200.0;
                        if let Some(f) = c.nearest_floor(pp[0], pp[1], pp[2], REGROUND_UP, 0.0) {
                            self.scene.player_pos[2]  = f;
                            self.visual_player_pos[2] = f;
                            self.camera.focus[2]      = f;
                            if let Some(ref mut op) = self.override_pos { op[2] = f; }
                            self.vert_vel  = 0.0;
                            self.on_ground = true;
                            self.needs_reground = false;
                            tracing::info!("zone-in: regrounded player from {:.1} up to floor z={:.1}", pp[2], f);
                        }
                        // else: collision not ready / no floor found yet — retry next frame.
                    } else {
                        // Already at/above a floor — let normal physics settle; nothing to recover.
                        self.needs_reground = false;
                    }
                }
            }

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
            // For nav-driven movement (/goto, auto-engage) use the nav thread's authoritative
            // heading directly: it is set from the direction of each movement step and is more
            // reliable than motion-vector derivation (the visual glide introduces a lag that
            // can misalign heading at corners or during the first frame of a new step).
            // `game_state.player_heading` is kept live by the nav thread's synthetic position
            // packets (make_position_packet → apply_position_update); without that it would be
            // the stale spawn heading and this would snap the facing away from travel.
            // Only applies when there is recent movement and no keyboard input.
            if !manual_move && self.last_moved_at.elapsed().as_millis() < 300 {
                self.heading_target = self.game_state.player_heading;
            }
            self.prev_render_pos = self.scene.player_pos;
            // When rotating with A/D or steering with the mouse (drive), snap visual_heading
            // immediately for responsive feel. When following motion, lerp to avoid nav jitter.
            if rotating || lmb_drive {
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

        let dur_update = prof_update.elapsed();

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
            Err(e) => { tracing::error!("surface error: {e}"); return; }
        };
        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = renderer.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("frame") },
        );

        let prof_render = crate::profiling::Stopwatch::start();
        renderer.render_frame(&mut enc, &view, &self.scene, cam_eye, cam_target, dt);
        let dur_render = prof_render.elapsed();

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
        let sync_frac = *self.sync_progress.lock().unwrap();
        let prof_egui = crate::profiling::Stopwatch::start();
        Self::egui_pass(
            &mut self.egui_state, &mut self.egui_renderer, &self.egui_ctx, &mut self.ui_layout, &self.window,
            &mut enc, &view, renderer, self.loading, &self.current_zone, &load_status_text,
            sync_frac,
            &self.scene, self.zone_min, self.zone_max,
            &mut self.minimap_zoom, &mut self.minimap_full, &mut self.show_map,
            self.current_fps, self.zone_map.as_ref(),
            cam_eye, self.collision.as_deref(),
            &self.hail, &self.say, &self.target, &mut self.say_buffer,
            &self.attack, &self.cast, &self.sit, &self.consider, &self.spells,
            &self.buy, &self.sell, &self.trade,
            &self.spell_icons,
            &mut self.show_inventory,
            &mut self.ui_zoom, &mut self.ui_zoom_size,
            self.show_debug, self.game_state.server_corrections,
            &self.frame_profile,
            &self.camp, &self.camp_until,
        );
        let dur_egui = prof_egui.elapsed();

        // Submit — associated function avoids reborrowing self.
        let prof_submit = crate::profiling::Stopwatch::start();
        Self::submit_frame(&self.frame_req, enc, output, renderer);
        let dur_submit = prof_submit.elapsed();

        // Record per-phase timings for the --profile HUD overlay (cheap; only blended when enabled).
        if crate::profiling::enabled() {
            let sample = crate::profiling::FrameSample {
                update: dur_update,
                render: dur_render,
                egui:   dur_egui,
                submit: dur_submit,
                total:  now.elapsed(),
            };
            self.frame_profile.blend(&sample, frame_ms);
        }

        // NOTE: no `request_redraw()` here. The loop is event-driven — `about_to_wait` decides whether
        // the next frame is needed (active animation/input/packets) and only then requests a redraw.
        // A still scene therefore stops rendering and idle CPU drops to ~0. See `about_to_wait`/`wake`.
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
        sync_progress: Option<f32>,
        scene:         &SceneState,
        zone_min:      [f32; 2],
        zone_max:      [f32; 2],
        minimap_zoom:  &mut f32,
        minimap_full:  &mut bool,
        show_map:      &mut bool,
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
        buy:           &crate::http::BuyReq,
        sell:          &crate::http::SellReq,
        trade:         &crate::http::TradeReq,
        spell_icons:   &[egui::TextureHandle],
        show_inventory: &mut bool,
        ui_zoom:       &mut f32,
        ui_zoom_size:  &mut (u32, u32),
        show_debug:    bool,
        corrections:   u32,
        frame_profile: &crate::profiling::FrameProfile,
        camp:          &crate::http::CampReq,
        camp_until:    &crate::http::CampUntil,
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
            if crate::profiling::enabled() {
                hud::draw_profile(ctx, frame_profile);
            }
            if loading {
                hud::draw_loading(ctx, current_zone, load_status, sync_progress);
            } else {
                hud::draw_ui_menu(ctx, ui_layout);
                hud::draw_hud(ctx, ui_layout, scene, "EQ Observer");
                hud::draw_quest_dialogue(ctx, ui_layout, scene, say);
                hud::draw_message_log(ctx, ui_layout, scene);
                hud::draw_labels(ctx, scene, view_proj, screen_w, screen_h, cam_eye, collision);
                hud::draw_minimap(ctx, ui_layout, scene, zone_min, zone_max, minimap_zoom, minimap_full, zone_map, show_map);
                hud::draw_control_bar(ctx, ui_layout, scene, hail, say, target, say_buffer, camp, camp_until);
                hud::draw_action_grid(ctx, ui_layout, scene, spells, spell_icons, attack, cast, sit, target, consider);
                hud::draw_inventory(ctx, ui_layout, scene, show_inventory);
                hud::draw_merchant(ctx, ui_layout, scene, buy, sell, trade);
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
        // Kick the event-driven loop: render the first frames so zone loading starts (in --testzone
        // there are no network packets to trigger it). Once loading sets in, `poll_external` keeps the
        // loop active on its own.
        self.wake();
    }

    /// Called each loop iteration before winit waits for events. Two jobs:
    ///
    /// 1. Honour shutdown: if a shutdown was requested (POST /exit or OP_GMKick set the flag), exit the
    ///    event loop HERE on the main thread so winit shuts down its Wayland clipboard worker cleanly.
    ///    A background thread calling `process::exit()` while that worker is live races its Wayland-
    ///    object teardown → SIGSEGV.
    ///
    /// 2. Drive the event-driven render schedule: drain the network channel, and if anything is in
    ///    flight (packets, input, animation, a queued request) render at ~60fps for a short linger
    ///    window; otherwise drop to a cheap idle poll so a still scene costs ~no CPU. This replaces the
    ///    old `ControlFlow::Poll` + unconditional `request_redraw()` busy loop that pegged a core even
    ///    when the character stood still.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.shutdown.load(std::sync::atomic::Ordering::Relaxed) {
            event_loop.exit();
            return;
        }

        // Drain packets + detect in-flight activity. Any activity extends the active render window.
        if self.poll_external() {
            self.active_until = std::time::Instant::now() + Self::ACTIVE_LINGER;
        }

        // Keep rendering while a camp is in progress so the HUD countdown ticks smoothly even in a
        // still scene (the event-driven loop would otherwise idle between sparse packets).
        if self.camp_until.lock().unwrap().is_some() {
            self.active_until = std::time::Instant::now() + Self::ACTIVE_LINGER;
        }

        let now = std::time::Instant::now();
        if now < self.active_until {
            // Active: schedule another frame at ~60fps.
            if let Some(w) = &self.window { w.request_redraw(); }
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Self::FRAME_INTERVAL));
        } else {
            // Idle: no render. Wake periodically only to poll the network channel; near-zero CPU.
            event_loop.set_control_flow(ControlFlow::WaitUntil(now + Self::IDLE_POLL));
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _id:        winit::window::WindowId,
        event:      WindowEvent,
    ) {
        // Handle RedrawRequested FIRST — before egui sees it. egui's `on_window_event` returns
        // `repaint = true` for a RedrawRequested, so feeding it there would call `wake()` →
        // `request_redraw()` → another RedrawRequested … an unbreakable 60fps loop that defeats the
        // whole event-driven scheme. Rendering also never needs egui to "consume" a redraw request.
        if let WindowEvent::RedrawRequested = event {
            self.render_frame();
            // Defer zone reload until after the GPU borrow is fully released.
            if mem::take(&mut self.pending_reload) {
                self.reload_zone();
            }
            // Background load thread finished? Do the GPU upload. Asset sync finished? Load models.
            self.maybe_finish_load();
            self.poll_sync();
            return;
        }

        // Let egui see the event first. If it wants a repaint (hover/focus/typing) or consumes the
        // event, wake the loop so the UI updates; bail out on consumed events.
        let egui_resp = if let (Some(egui_state), Some(window)) = (&mut self.egui_state, &self.window) {
            Some(egui_state.on_window_event(window, &event))
        } else {
            None
        };
        if let Some(resp) = egui_resp {
            if resp.repaint { self.wake(); }
            if resp.consumed { return; }
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
                                match self.pick_at(self.last_cursor) {
                                    Some(PickResult::Entity(id)) => {
                                        self.game_state.target_id   = Some(id);
                                        self.game_state.target_con  = None;
                                        if let Some(e) = self.game_state.entities.get(&id) {
                                            self.game_state.target_name   = Some(e.name.clone());
                                            self.game_state.target_hp_pct = Some(e.hp_pct);
                                        }
                                        *self.target.lock().unwrap() = Some(id);
                                    }
                                    Some(PickResult::Door(door_id)) => {
                                        // Server-authoritative: only request the open; never set is_open locally.
                                        *self.door_click.lock().unwrap() = Some(door_id);
                                        self.game_state.log_msg("door",
                                            &format!("Clicked door {}", door_id));
                                    }
                                    None => {}
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
                                    // Manual movement cancels any in-progress /goto so WASD takes
                                    // over immediately (jump/crouch don't count as movement).
                                    if matches!(code, KeyCode::KeyW | KeyCode::KeyA | KeyCode::KeyS
                                        | KeyCode::KeyD | KeyCode::KeyQ | KeyCode::KeyE)
                                    {
                                        *self.goto_target.lock().unwrap() = None;
                                    }
                                }
                                KeyCode::KeyR | KeyCode::F9 => {
                                    self.camera.reset_to_follow();
                                    self.override_pos = None;
                                    *self.goto_target.lock().unwrap() = None;
                                }
                                KeyCode::F10 => {
                                    self.show_debug = !self.show_debug;
                                    tracing::info!("DEBUG: overlay {}", if self.show_debug { "ON" } else { "OFF" });
                                }
                                KeyCode::KeyI => {
                                    self.show_inventory = !self.show_inventory;
                                }
                                KeyCode::KeyM => {
                                    self.show_map = !self.show_map;
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

        // Any non-redraw event that reached here (input, resize, focus, …) may change what's drawn, so
        // render at least one frame and keep the active window open briefly for follow-up animation.
        self.wake();
    }
}

/// Decide whether the zone geometry must be (re)loaded.
///
/// `scene_zone` is the zone the player is currently standing in (from the latest scene snapshot);
/// `current_zone` is the zone whose geometry we last started loading. A reload is needed exactly
/// when they differ — a durable *level* condition that, unlike the transient `zone_changed` edge
/// flag, cannot be missed by packet-drain timing (see the call site for the race this avoids).
///
/// An empty `scene_zone` (no zone yet, or a transient reset) never triggers a load: there is no
/// `<empty>.glb` to fetch, and loading it would only blow away real terrain for a fallback plane.
fn zone_needs_reload(scene_zone: &str, current_zone: &str) -> bool {
    !scene_zone.is_empty() && scene_zone != current_zone
}

#[cfg(test)]
mod tests {
    use super::zone_needs_reload;

    #[test]
    fn first_zone_in_triggers_load() {
        // current_zone starts empty; arriving in a real zone must load it.
        assert!(zone_needs_reload("arena", ""));
    }

    #[test]
    fn changing_zones_triggers_load() {
        assert!(zone_needs_reload("gfaydark", "arena"));
    }

    #[test]
    fn same_zone_does_not_reload() {
        // Already loaded: re-snapshotting the same zone must not thrash a reload.
        assert!(!zone_needs_reload("arena", "arena"));
    }

    #[test]
    fn empty_scene_zone_never_loads() {
        // No zone yet / transient reset: don't try to fetch `<empty>.glb` over a loaded zone.
        assert!(!zone_needs_reload("", ""));
        assert!(!zone_needs_reload("", "arena"));
    }
}
