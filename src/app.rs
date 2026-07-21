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
use crate::frame_capture::encode_frame_png;
use crate::game_state::GameState;

use crate::ipc::FrameReq;
use crate::renderer::EqRenderer;
use crate::scene::SceneState;
use crate::nav::collision;
use crate::{assets, debug_zone, hud, zone_map};

/// Data produced by the background zone-load thread, ready for GPU upload on the main thread.
struct PendingLoad {
    zone_name: String,
    /// None means the S3D failed to load; use the fallback ground plane instead.
    assets:    Option<assets::ZoneAssets>,
    collision: Option<Arc<collision::Collision>>,
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
    /// Memoized floor snap: the (smoothed) position `floor_z` was raycast at, NaN when invalid.
    /// A stationary entity's display position settles to exact bit-equality, so comparing the
    /// current position against this skips the downward floor raycast entirely for entities that
    /// haven't moved — the bulk of a parked scene — instead of re-raycasting all of them at 60fps
    /// (#152). Recomputed whenever the position changes at all.
    floor_at:    [f32; 3],
    /// Cached result of `Collision::floor_z` at `floor_at`.
    floor_z:     f32,
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
    /// Zone-transition fade (#286): 0.0 = clear, 1.0 = fully black. Ramps to black fast when a
    /// zone/position change commits (hiding the reposition + old-scene-then-pop), holds black while
    /// the new zone loads, and fades back in once it's ready — so all three relocation paths (zone
    /// transfer, summon, death→bind) get one clean transition instead of an abrupt cut.
    fade:           f32,
    /// Current loading step shown to the user while loading == true.
    load_status:    Arc<Mutex<String>>,
    /// Background thread writes completed load data here; render loop drains it.
    pending_load:   Arc<Mutex<Option<PendingLoad>>>,
    // Minimap
    zone_min:      [f32; 2],
    zone_max:      [f32; 2],
    zone_map:      Option<zone_map::ZoneMap>,
    // Camera & smooth position
    visual_player_pos:  [f32; 3],
    prev_logical_pos:   [f32; 3],
    last_moved_at:      std::time::Instant,
    camera:             CameraState,
    camera_cmd:         Arc<Mutex<Option<CameraCmd>>>,
    camera_snapshot:    Arc<Mutex<CameraSnapshot>>,
    /// The HTTP manual-move/jump escape hatch (`POST /v1/move/{manual,jump}`), read directly here
    /// each frame. MVC C2 (#452): this is a view→RENDER command owned by `ipc::CameraSlots` (not the
    /// view→model `CommandState`), so `App` holds the slot itself rather than reaching through
    /// `self.acts.command`. The DERIVED heading it implies is computed render-side (`manual_wish`).
    manual_move:        crate::ipc::ManualMoveReq,
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
    /// Single-authority character controller (Component A): sole owner of the local player's
    /// physical state. Its position drives both the render and (via `controller_view`) the server
    /// stream. Replaces the old `override_pos` dual-authority that caused WASD rubber-banding.
    controller:       crate::movement::CharacterController,
    /// Snapshot published each frame for the nav thread to stream.
    controller_view:  crate::ipc::ControllerShared,
    /// The nav planner's /goto movement intent, consumed when no WASD key is held.
    nav_intent:       crate::ipc::NavIntent,
    /// A large server correction handed over by the nav streamer; applied to the controller.
    pos_correction:   crate::ipc::PosCorrection,
    /// Walker's live plan (coarse, fine), published by the nav thread; drawn by the nav-debug
    /// overlay so it shows what the walker actually follows, not a separate recompute (#246).
    nav_path_view:    crate::ipc::NavPathView,
    /// Shared goto target, kept ONLY as a read handle for the nav-debug overlay (`egui_pass`'s
    /// `goto_target` param) — the render thread no longer WRITES this directly; cancelling a goto
    /// goes through `self.acts.command.request_cancel_goto()` (#459), which shares this same Arc.
    goto_target:  crate::ipc::GotoTarget,
    /// All shared request slots UI windows write; the nav/gameplay threads drain
    /// and send them. One struct instead of a dozen fields (#162).
    acts:         crate::ui::Actions,
    spells:       std::sync::Arc<crate::spells::SpellDb>,
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
    /// The `ArcSwap` handle the network thread publishes into every gameplay tick.
    game_state_snapshot: crate::ipc::GameStateSnapshot,
    /// This frame's cached load of `game_state_snapshot`. Refreshed at the top of poll_external
    /// and render_frame; reads between two refresh points may straddle two snapshots, which is
    /// fine — each snapshot is internally consistent.
    game_state_view: std::sync::Arc<GameState>,
    /// Render-thread-owned door open/close easing state, keyed by `door_id`. `GameState::Door`
    /// only carries the authoritative `is_open`; this map is what actually animates the swing.
    door_frac: std::collections::HashMap<u8, f32>,
    /// Offline testzone mode — bypasses EQ server entirely.
    #[allow(dead_code)]
    testzone_mode: bool,
    /// Set by every shutdown path (POST /exit, OP_GMKick). Observed in `about_to_wait` to exit the
    /// winit event loop on the MAIN thread, so winit tears down its Wayland clipboard worker cleanly
    /// — instead of a background thread calling `process::exit()` and racing that teardown (SIGSEGV).
    shutdown:     std::sync::Arc<std::sync::atomic::AtomicBool>,
    scene:        SceneState,
    /// When an inbound server packet was last applied. Feeds the connection-health signal
    /// (`connected`/`last_packet_age_ms`) so a dead/frozen server is distinguishable from an idle
    /// one instead of the world silently freezing (eqoxide#8).
    last_inbound: std::time::Instant,
    /// The network thread's live "time of last real inbound packet" handle — polled once per
    /// `poll_external` and compared against `last_inbound` to detect a fresh arrival.
    net_health: crate::ipc::NetHealthShared,
    // Frame capture for /frame API
    frame_req:    FrameReq,
    /// Smoothed per-phase frame timings, published for `/v1/observe/debug` → `frame_profile`.
    /// This is the ONLY agent-facing value the render loop publishes: everything else an agent reads
    /// is projected at HTTP read time from the network thread's `GameState` (#343). Publishing world
    /// state from a loop whose whole design goal is to STOP RUNNING when nothing is happening is how
    /// `connected: true` survived a dead connection forever.
    frame_profile_shared: crate::ipc::FrameProfileShared,
    // Precomputed zone collision grid: floor grounding, camera collision, nameplate occlusion.
    // Held as Arc and also published to `shared_collision` so the nav thread can read it.
    collision:    Option<Arc<collision::Collision>>,
    /// Shared slot the nav thread reads to gate /goto movement against walls.
    shared_collision: collision::SharedCollision,
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
    /// Navmesh/pathfinding debug overlay (collision floor grid + live A* path). Initial
    /// state from `--nav-debug`; F11 toggles at runtime. See `hud::draw_nav_debug`.
    nav_debug: bool,
    /// The window system: registry-driven windows, per-character layout
    /// persistence, icon atlases, chat state (#162).
    ui_state: crate::ui::UiState,
    /// Asset-sync progress fraction (0.0–1.0) shown on the loading screen; None when not syncing.
    sync_progress: std::sync::Arc<std::sync::Mutex<Option<f32>>>,
    /// Set to Some(Ok(())) when the common-model sync finishes, Some(Err(msg)) on failure.
    sync_done: std::sync::Arc<std::sync::Mutex<Option<Result<(), String>>>>,
    /// True once character models have been loaded from the cache (guards one-time load).
    models_loaded: bool,
    asset_server_url: String,
    asset_user: String,
    asset_pass: String,
    /// OS window title — "{account} {character} - EQOxide" so side-by-side agent clients are
    /// tellable apart on the taskbar/switcher (#297). Computed once at construction from config.
    window_title: String,
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
        manual_move:     crate::ipc::ManualMoveReq,
        game_state_snapshot: crate::ipc::GameStateSnapshot,
        net_health: crate::ipc::NetHealthShared,
        frame_req:       FrameReq,
        goto_target:     crate::ipc::GotoTarget,
        acts:            crate::ui::Actions,
        spells:          std::sync::Arc<crate::spells::SpellDb>,
        shared_collision: collision::SharedCollision,
        frame_profile_shared: crate::ipc::FrameProfileShared,
        testzone_mode:   bool,
        nav_debug:       bool,
        shutdown:        std::sync::Arc<std::sync::atomic::AtomicBool>,
        eq_ui_dir:       Option<String>,
        asset_server_url: String,
        asset_user:       String,
        asset_pass:       String,
        controller_view:  crate::ipc::ControllerShared,
        nav_intent:       crate::ipc::NavIntent,
        pos_correction:   crate::ipc::PosCorrection,
        nav_path_view:    crate::ipc::NavPathView,
    ) -> Self {
        let ui_state = crate::ui::UiState::new(&character_name, eq_ui_dir);
        // Distinct per-client window title (#297): "{account} {character} - EQOxide".
        let window_title = format!("{} {} - EQOxide", asset_user, character_name);
        if testzone_mode {
            // No network thread runs in --testzone mode (it's skipped entirely in main.rs), so
            // nothing else will ever publish into `game_state_snapshot` — it would otherwise sit
            // on the initial `GameState::new()` default forever. Seed it here so `game_state_view`
            // (what the scene build reads) sees the debug-zone bootstrap. Since #343 this seed also
            // backs `/v1/observe/debug` (which projects the player view straight off this snapshot);
            // `render_frame` then republishes it each frame with the live controller position, so
            // offline mode reports a moving player rather than a frozen seed. `connected` is
            // correctly false throughout — there is genuinely no connection.
            let mut gs = GameState::new();
            gs.player_name = character_name.clone();
            gs.world.zone_name = "testzone".to_string();
            gs.world.zone_changed = true;
            game_state_snapshot.store(std::sync::Arc::new(gs));
            tracing::info!("APP: --testzone mode, will load debug zone");
        }
        let game_state_view = game_state_snapshot.load_full();

        App {
            window: None, gpu: None, egui_ctx: None, egui_state: None, egui_renderer: None,
            models_path,
            current_zone: String::new(), loading: false, pending_reload: false, fade: 0.0,
            load_status:  Arc::new(Mutex::new(String::new())),
            pending_load: Arc::new(Mutex::new(None)),
            zone_min: [0.0; 2], zone_max: [0.0; 2],
            zone_map: None,
            visual_player_pos: [0.0, 0.0, 0.0],
            prev_logical_pos:  [0.0, 0.0, 0.0],
            last_moved_at:     std::time::Instant::now(),
            camera: CameraState::new([0.0, 0.0, 0.0], 0.0),
            camera_cmd, camera_snapshot, manual_move,
            camera_initialized: false,
            needs_reground: false,
            last_frame_time: std::time::Instant::now(),
            fps_frame_count: 0,
            fps_timer: std::time::Instant::now(),
            current_fps: 0.0,
            active_until: std::time::Instant::now(),
            frame_profile: crate::profiling::FrameProfile::default(),
            keys_held: std::collections::HashSet::new(),
            controller: crate::movement::CharacterController::new([0.0, 0.0, 0.0]),
            controller_view, nav_intent, pos_correction, nav_path_view,
            goto_target,
            acts, spells,
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
            scene: SceneState::default(), last_inbound: std::time::Instant::now(), frame_req,
            frame_profile_shared, shutdown, collision: None, shared_collision,
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
            nav_debug,
            ui_state,
            sync_progress: Arc::new(Mutex::new(None)),
            sync_done:     Arc::new(Mutex::new(None)),
            models_loaded: false,
            asset_server_url, asset_user, asset_pass,
            window_title,
            game_state_snapshot, game_state_view, net_health,
            door_frac: std::collections::HashMap::new(),
        }
    }

    /// Record the OS window's current geometry into the per-character layout
    /// (debounced by the layout's save machinery). Position is best-effort:
    /// `outer_position()` errors on Wayland, in which case only size/maximized
    /// round-trip (#162).
    fn record_os_window(&mut self) {
        let Some(window) = &self.window else { return };
        let size = window.inner_size();
        if size.width == 0 || size.height == 0 {
            return;
        }
        let maximized = window.is_maximized();
        let pos = window.outer_position().ok().map(|p| [p.x, p.y]);
        // While maximized, keep the last floating size/pos on record so
        // un-maximizing next session restores a sensible window instead of a
        // monitor-sized one; only the flag updates.
        let prev = self.ui_state.layout().os_window;
        let st = if maximized {
            let prev = prev.unwrap_or(crate::ui::persist::OsWindowState {
                size: [size.width, size.height],
                pos,
                maximized: true,
            });
            crate::ui::persist::OsWindowState { maximized: true, ..prev }
        } else {
            crate::ui::persist::OsWindowState { size: [size.width, size.height], pos, maximized }
        };
        self.ui_state.layout_mut().set_os_window(st);
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

        for (&id, e) in &self.game_state_view.world.entities {
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
        for d in self.game_state_view.world.doors.values() {
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

        // Named for the #380 crash-log panic hook — see `crash` module docs.
        std::thread::Builder::new().name("zone-asset-loader".into()).spawn(move || {
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
            let water = crate::region_map::RegionMap::load(&maps_dir.join("water"), &zone_name).map(Arc::new);
            let collision = opt_assets.as_ref().map(|za| {
                let mut c = collision::Collision::build(za, 32.0);
                c.set_water(water);
                Arc::new(c)
            });

            set_status("Loading minimap…");
            let zone_map = zone_map::ZoneMap::load(&maps_dir, &zone_name);

            set_status("Uploading to GPU…");
            *pending.lock().unwrap() = Some(PendingLoad {
                zone_name, assets: opt_assets, collision, zone_map, zone_min, zone_max,
            });
        }).expect("spawn zone-asset-loader thread");
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
        let mut renderer  = EqRenderer::new(device, queue, surface_config);
        // Resolve models to the cwd-independent XDG cache and sync the `common`
        // set from the asset server before loading character models.
        let cache = crate::asset_sync::CacheDirs::resolve();

        // Background model-sync worker (eqoxide#224): the ~450 MB of playable-race models are no
        // longer in the startup `common` set — each is its own `charmodel/<key>` set fetched on
        // demand the first time a spawn of that race is rendered. The renderer sends a race key
        // here; this worker logs in once and syncs that set, then the lazy loader picks it up.
        {
            let (model_tx, model_rx) = std::sync::mpsc::channel::<String>();
            let url = self.asset_server_url.clone();
            let user = self.asset_user.clone();
            let pass = self.asset_pass.clone();
            std::thread::Builder::new().name("model-sync-worker".into()).spawn(move || {
                let wcache = crate::asset_sync::CacheDirs::resolve(); // same XDG path; cheap
                let sync = match crate::asset_sync::AssetSync::login(&url, &user, &pass) {
                    Ok(s) => s,
                    Err(e) => { tracing::warn!("model-sync worker: login failed: {e}"); return; }
                };
                while let Ok(key) = model_rx.recv() {
                    let set = format!("charmodel/{key}");
                    match crate::asset_sync::sync_set(&sync, &set, &wcache, &mut |_| {}) {
                        Ok(()) => tracing::debug!("model-sync worker: synced {set}"),
                        Err(e) => tracing::warn!("model-sync worker: sync {set} failed: {e}"),
                    }
                }
            }).expect("spawn model-sync-worker thread");
            renderer.set_model_sync_tx(model_tx);
        }
        self.models_path = cache.models_dir();
        self.loading = true;
        *self.load_status.lock().unwrap() = "Connecting to asset server…".to_string();

        let url = self.asset_server_url.clone();
        let user = self.asset_user.clone();
        let pass = self.asset_pass.clone();
        let status = self.load_status.clone();
        let progress = self.sync_progress.clone();
        let done = self.sync_done.clone();
        std::thread::Builder::new().name("common-asset-loader".into()).spawn(move || {
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
        }).expect("spawn common-asset-loader thread");
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
    /// render must service (frame capture / camera).
    fn poll_external(&mut self) -> bool {
        let mut activity = false;
        // `publish_snapshot` (eq_net::gameplay) only stores a new Arc into `game_state_snapshot`
        // when the freshly-mutated `GameState` actually differs (PartialEq) from what's already
        // published, so the Arc's pointer identity is now a COMPLETE activity signal: it covers
        // both a real inbound packet (apply_packet) and a client-initiated mutation that produced
        // no packet at all (e.g. ActionLoop::tick handling POST /v1/interact/sit, or the auto-loot
        // session-close timer). A genuinely idle world republishes the same Arc, so this correctly
        // lets the render loop sleep.
        let new_view = self.game_state_snapshot.load_full();
        if !std::sync::Arc::ptr_eq(&new_view, &self.game_state_view) {
            activity = true;
        }
        self.game_state_view = new_view;

        // Connection health (`connected` / CONN_STALE_SECS / the "connection lost" banner) stays
        // strictly packet-based — it must NOT be driven by the activity signal above, which now
        // also fires for packet-less client-initiated changes. `last_inbound_shared` is bumped only
        // where a real inbound packet is applied (gameplay.rs's drain loop, login.rs, and the
        // zone/world reconnect handshakes), so mirror it here purely for the elapsed-time checks
        // further down — it does not gate `activity`.
        // The HUD banner tracks LINK liveness (any inbound datagram), not application traffic —
        // an idle world legitimately sends no app packets for 40+s and is not disconnected (#343).
        let new_inbound = self.net_health.lock().unwrap().last_datagram;
        if new_inbound != self.last_inbound {
            self.last_inbound = new_inbound;
        }
        // The HUD's "connection lost" banner is rendered, so it needs a frame to appear — and a dead
        // connection produces no packets, hence no activity, hence no frame. Wake once whenever the
        // health state flips so the human sees the banner (the API no longer depends on this: since
        // #343 `connected` is derived at HTTP read time and needs no render at all).
        if (self.last_inbound.elapsed().as_secs() >= crate::http::CONN_STALE_SECS) != self.scene.disconnected {
            activity = true;
        }

        // Still loading a zone, or a reload is queued → keep rendering the progress screen.
        if self.loading || self.pending_reload { activity = true; }

        // A queued HTTP request that only a render frame can service.
        if self.frame_req.lock().is_ok_and(|g| g.is_some()) { activity = true; }
        if self.camera_cmd.lock().is_ok_and(|g| g.is_some()) { activity = true; }

        // A pending server position correction (GM #summon, knockback, spell pushback, anti-cheat
        // snap) is consumed only inside the render frame (`pos_correction` handler → controller
        // teleport). Force a frame even when the client is otherwise idle so the controller adopts
        // the new position promptly; otherwise the correction sits unconsumed while the position
        // streamer re-sends the stale controller position, reverting both client and server (#116).
        if self.pos_correction.lock().is_ok_and(|g| g.is_some()) { activity = true; }

        // Player input / motion in flight (keys held, free-fly override active, or falling).
        let nav_driving = self.nav_intent.lock().map(|g| g.is_some()).unwrap_or(false);
        if !self.keys_held.is_empty() || nav_driving || !self.on_ground {
            activity = true;
        }

        // Doors still easing toward their open/closed target.
        if self.game_state_view.world.doors.iter().any(|(id, d)| {
            let target = if d.is_open { 1.0 } else { 0.0 };
            let frac = self.door_frac.get(id).copied().unwrap_or(target);
            (frac - target).abs() > 0.001
        }) {
            activity = true;
        }

        // Visual position still gliding toward the logical (server-authoritative) position.
        let dx = self.game_state_view.player_x - self.visual_player_pos[0];
        let dy = self.game_state_view.player_y - self.visual_player_pos[1];
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
        self.game_state_view = self.game_state_snapshot.load_full();
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

        // #326: clear any stale door_frac entries from the OLD zone before they're read below.
        // This must run against the fresh `game_state_view` (not `self.scene`, which isn't
        // rebuilt until after the easing loop) — otherwise this frame's scene is built from the
        // old zone's fractions and a door flashes at the previous zone's open/closed state for
        // one frame. The full reload bookkeeping (collision drop, pending_reload, etc.) still
        // runs later against `self.scene.zone`; this is just the door_frac clear pulled earlier
        // so it beats the read below. See `reset_door_frac_on_zone_change`.
        reset_door_frac_on_zone_change(&mut self.door_frac, &self.game_state_view.world.zone_name, &self.current_zone);

        // Ease each door's render-only open fraction toward its server-authoritative open/close
        // target. Lives on App (not GameState) — see `ease_door_frac`. New doors seed at their
        // current state (a door that spawns open renders open immediately, matching the old
        // spawn-time open_frac init) — only subsequent state *changes* animate.
        for (&id, d) in self.game_state_view.world.doors.iter() {
            let entry = self.door_frac.entry(id)
                .or_insert_with(|| if d.is_open { 1.0 } else { 0.0 });
            *entry = ease_door_frac(*entry, d.is_open, dt, DOOR_TRAVEL_SECS);
        }
        self.door_frac.retain(|id, _| self.game_state_view.world.doors.contains_key(id));

        let prof_scene = crate::profiling::Stopwatch::start();
        self.scene = SceneState::from_game_state(&self.game_state_view, &self.door_frac);
        let dur_scene = prof_scene.elapsed();

        // Publish the render loop's ONLY agent-facing output: this frame's smoothed phase timings.
        // Everything else the agent reads (`/v1/observe/debug`'s player block, `connected`,
        // `last_packet_age_ms`) is now projected at HTTP read time from the network thread's
        // GameState + the two liveness clocks (#343). It used to be published from right here — a
        // loop that deliberately sleeps when no packets arrive — so a dead connection meant
        // `connected` was never recomputed and reported `true`, frozen, forever.
        *self.frame_profile_shared.lock().unwrap() = self.frame_profile;

        // `--testzone` runs with NO network thread, so nothing else ever writes the GameState
        // snapshot the API projects from — the reported position would otherwise stay frozen at
        // App::new's seed forever (#343 review). Offline, the render loop IS the sole owner of
        // GameState, so it publishes here. This is not a re-coupling of observation to rendering:
        // in this mode there is no other owner, and `connected` stays honestly false (no datagram
        // ever arrives) while `snapshot_age_ms` stays fresh.
        if self.testzone_mode && self.camera_initialized {
            let mut gs = (*self.game_state_view).clone();
            gs.player_x       = self.controller.pos[0];
            gs.player_y       = self.controller.pos[1];
            gs.player_z       = self.controller.pos[2];
            gs.player_heading = self.visual_heading;
            crate::eq_net::gameplay::publish_snapshot(
                &gs, &self.game_state_snapshot, &self.net_health);
        }
        // Mirror the health state into the scene so the HUD can show a "connection lost" banner (#8).
        self.scene.disconnected = self.last_inbound.elapsed().as_secs() >= crate::http::CONN_STALE_SECS;

        // In the test zone, inject fake billboards so every loaded character model
        // is rendered side-by-side for visual debugging.
        if self.scene.zone == "testzone" {
            self.scene.inject_test_billboards();
        }

        // Smooth NPC movement + snap billboards to the terrain floor — gated by distance so the
        // per-frame cost scales with NEARBY spawns, not total zone population (#152).
        let prof_smooth = crate::profiling::Stopwatch::start();
        smooth_entity_motion(
            &mut self.entity_motion,
            &mut self.scene.billboards,
            self.scene.player_pos,
            self.collision.as_deref(),
            std::time::Instant::now(),
            dt,
        );
        let dur_smooth = prof_smooth.elapsed();

        // Detect movement from the logical (server-authoritative) position.
        // Nav steps fire every 150 ms; we latch "moving" for 250 ms so the
        // walking animation runs continuously between steps rather than flickering.
        {
            let lp = [self.game_state_view.player_x, self.game_state_view.player_y, self.game_state_view.player_z];
            let dx = lp[0] - self.prev_logical_pos[0];
            let dy = lp[1] - self.prev_logical_pos[1];
            let dz = lp[2] - self.prev_logical_pos[2];
            let nav_dist = (dx * dx + dy * dy).sqrt();
            if nav_dist > 0.01 {
                // Estimate nav-driven speed from the distance moved over the elapsed interval.
                // Clamped to [50ms, 500ms] so a stale first frame doesn't spike the estimate.
                let dt_upd = (now - self.last_player_nav_update).as_secs_f32().clamp(0.05, 0.5);
                self.player_nav_speed = nav_dist / dt_upd;
                self.last_player_nav_update = now;
            }
            // `last_moved_at` latches "moving" for the animation. Count VERTICAL swim too (in water)
            // so swimming straight up/down with no horizontal travel still plays the swim clip —
            // otherwise a diving/surfacing character reads as idle (#207 companion to the #198 anim).
            if nav_dist > 0.01 || (self.controller.in_water && dz.abs() > 0.01) {
                self.last_moved_at = std::time::Instant::now();
            }
            self.prev_logical_pos = lp;
            // Priority: dead > combat swing > walking > sitting > idle. Combat and
            // movement override sitting (classic EQ stands you up when you attack or
            // move); sitting only replaces the plain idle clip. (eqoxide#53)
            let pid = self.game_state_view.player_id;
            let player_dead = self.game_state_view.cur_hp <= 0 && self.game_state_view.max_hp > 0;
            let swinging = self.game_state_view.combat_anims.get(&pid)
                .map_or(false, |(_, t)| t.elapsed() < crate::scene::COMBAT_SWING_WINDOW);
            self.scene.player_action = if player_dead {
                "dead".to_string()
            } else if let Some((code, _)) = self.game_state_view.combat_anims.get(&pid).filter(|_| swinging) {
                format!("C{:02}", code)
            } else if self.controller.in_water {
                // In water we always swim, never stand: the forward stroke (P06 "swim") while moving,
                // and treading water in place (L09 "swim_idle") when holding position — so a still
                // character doesn't appear to stand on the surface (#198/#207). in_water is the
                // controller's per-step check.
                if self.last_moved_at.elapsed().as_millis() < 250 { "swimming".to_string() } else { "treading".to_string() }
            } else if self.last_moved_at.elapsed().as_millis() < 250 {
                "walking".to_string()
            } else if self.game_state_view.sitting {
                "sitting".to_string()
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
            !self.camera_initialized && self.game_state_view.player_id != 0
        };
        if should_init_cam {
            self.visual_player_pos = self.scene.player_pos;
            self.heading_target    = self.scene.player_heading;
            self.visual_heading    = self.scene.player_heading;
            self.camera = CameraState::new(self.scene.player_pos, self.scene.player_heading);
            self.camera_initialized = true;
            // Seed the single-authority controller at the spawn position and mark it live so the nav
            // streamer begins mirroring/streaming it.
            self.controller.teleport(self.scene.player_pos);
            if let Ok(mut v) = self.controller_view.lock() {
                v.pos = self.scene.player_pos;
                v.heading = self.scene.player_heading;
                v.moving = false;
                v.initialized = true;
            }
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
            // `door_frac` is already cleared for this zone change above (#326) — that clear has
            // to run before the door-easing loop reads the map, which is earlier in this same
            // function than this reload block, so it isn't repeated here.
        }

        // Zone-transition fade (#286): drive `fade` toward black while a zone (re)load is committing
        // or in progress, and fade back in once the new zone is ready. Fast to black (~0.12s) so the
        // server-driven reposition + the old scene are hidden almost immediately (the client learns
        // the zone change and the new coords in the same packet, so we can't fade out *before* the
        // move — we black out as it commits); slower fade-in (~0.4s) for a smooth reveal of the new
        // zone. This covers all three relocation paths since they all funnel through the reload above
        // (cross-zone) — and a big same-zone reposition (summon/bind) is caught by `pending_reload`.
        self.fade = next_fade(self.fade, self.loading || self.pending_reload, dt);

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
            // Translate keys into a MoveIntent; the controller owns jump/gravity/collision/step-up.
            let wasd_active = de != 0.0 || dn != 0.0 || dz != 0.0;
            if wasd_active {
                // Manual movement CANCELS any in-progress /goto (native behavior; fixes the
                // "can't override a stalled nav" bug) before steering the controller this frame.
                self.acts.command.request_cancel_goto();
                *self.nav_intent.lock().unwrap() = None;
            }
            let space = self.keys_held.contains(&KeyCode::Space);
            // HTTP manual-move / jump escape hatch (#188): drive the controller like WASD when an
            // agent is stuck (A* found no path). Active only while within its deadline; yields to
            // real keyboard input, but takes priority over the nav planner's /goto intent.
            // Non-clearing per-frame poll of the view→render manual-move slot (#452: owned by
            // `ipc::CameraSlots`, not `CommandState`). `ManualMove` is `Copy`; the render loop
            // re-reads it every frame until its `until` deadline, so it must NOT drain.
            let manual = { *self.manual_move.lock().unwrap() }
                .filter(|m| std::time::Instant::now() < m.until);
            let intent = if wasd_active || space {
                crate::movement::MoveIntent {
                    wish_dir:    [de, dn],
                    wish_vspeed: if swimming { dz * MOVE_SPEED } else { 0.0 },
                    jump:        space,
                    want_swim:   swimming,
                    speed:       MOVE_SPEED,
                    climb:       0.0,   // free WASD uses the native 2u step (no wall-climbing)
                    hop:         false, // and does not auto-hop barriers (Space is the manual jump)
                }
            } else if let Some(m) = manual {
                // Like WASD, manual drive cancels any in-progress /goto so it doesn't fight us.
                self.acts.command.request_cancel_goto();
                *self.nav_intent.lock().unwrap() = None;
                let (wish, heading) = crate::movement::manual_wish(m.dir);
                if let Some(h) = heading { self.heading_target = h; } // face where we walk
                // Vertical control only applies in water: `up` swims up/down through the column, and
                // a jump underwater becomes full swim-up so /move/jump lifts a submerged character off
                // the pool floor. On land, jump is the normal hop and `up` is ignored (#207). Gate on
                // `in_water` (the player is in water), NOT the keyboard-swim `swimming` flag — that's
                // `lmb_drive && w_held`, which is never set for an API-driven agent.
                let vspeed = if in_water {
                    let v = m.up * MOVE_SPEED;
                    if m.jump && v < MOVE_SPEED { MOVE_SPEED } else { v }
                } else {
                    0.0
                };
                crate::movement::MoveIntent {
                    wish_dir:    wish,
                    wish_vspeed: vspeed,
                    jump:        m.jump && !in_water, // land hop only; underwater a jump is swim-up
                    want_swim:   in_water,
                    speed:       MOVE_SPEED,
                    climb:       0.0,
                    hop:         false,
                }
            } else {
                // No manual input → follow the nav planner's /goto intent (if any).
                self.nav_intent.lock().unwrap().unwrap_or_default()
            };

            // Apply a large server correction handed over by the nav streamer (design §3.4).
            if let Some(corr) = self.pos_correction.lock().unwrap().take() {
                self.controller.teleport(corr);
            }

            // One-shot reground after a zone change: if the controller spawned below the floor, lift
            // it onto the nearest floor once the new zone's collision is loaded.
            if self.needs_reground && !self.loading {
                if let Some(c) = self.collision.as_deref() {
                    let p = self.controller.pos;
                    if c.ground_below(p[0], p[1], p[2] + 1.0, 200.0).is_none() {
                        if let Some(f) = c.nearest_floor(p[0], p[1], p[2], 200.0, 0.0) {
                            self.controller.teleport([p[0], p[1], f]);
                            self.controller.on_ground = true;
                            self.needs_reground = false;
                            tracing::info!("zone-in: regrounded controller to floor z={:.1}", f);
                        }
                    } else {
                        self.needs_reground = false;
                    }
                }
            }

            // Integrate the controller (sole position authority). Step only once spawned and with
            // collision loaded; otherwise hold position so we don't fall through a loading void.
            if self.camera_initialized {
                if let Some(c) = self.collision.as_deref() {
                    // Keep the fall-through guard's threshold current with the zone's underworld
                    // floor (from OP_NewZone), so a collision gap can't drop us below it (#150).
                    self.controller.set_underworld(self.game_state_view.world.zone_underworld);
                    // #529: mirror the self-player's Levitate state so the controller floats (gravity
                    // off) instead of falling while the buff is up. Tracks the live buff as it is cast
                    // and fades; false for a normal grounded character (physics byte-identical).
                    self.controller.set_levitating(self.game_state_view.player_levitating);
                    self.controller.step(intent, dt, c);
                }
            }
            let cpos = self.controller.pos;
            self.on_ground         = self.controller.on_ground;
            self.vert_vel          = self.controller.vel_z;
            self.visual_player_pos = cpos;
            self.scene.player_pos  = cpos;
            self.camera.focus      = cpos;
            if self.on_ground { self.last_grounded_z = cpos[2]; }

            // Heading for nav-driven movement: face the planner's wish_dir (the render gs heading is
            // no longer kept live by synthetic packets). Manual facing is set by the heading block.
            if !manual_move {
                let wd = intent.wish_dir;
                if wd[0] * wd[0] + wd[1] * wd[1] > 1e-4 {
                    self.heading_target = crate::coord::eq_heading(wd[0], wd[1]);
                }
            }

            // Publish the controller's live position to the shared view EVERY frame. The nav thread
            // reads this to stream the position to the server AND to mirror into the network gs that
            // the /goto planner tracks progress against. Without this per-frame publish the view stays
            // frozen at the spawn position (set once at camera-init): the planner sees no progress,
            // skips every waypoint, and keeps driving the controller into a wall.
            //
            // Only publish once the controller has been seeded at the real spawn (camera-init). This
            // block runs every frame from the first — before camera-init, the controller isn't stepped
            // (see above) and `cpos` is its default ORIGIN. Publishing that would mark the view
            // `initialized` at (0,0,0), so the nav streamer sends a (0,0,0) OP_ClientUpdate before the
            // real spawn position is known — a 600+ unit jump the server flags as an MQWarp and then
            // corrects. Gating on `camera_initialized` lets the camera-init block do the first publish
            // with the real spawn position instead (#133).
            if self.camera_initialized {
                // Take the controller's one-shot landed-fall height (if it landed this frame) and
                // LATCH it into the view, so a single-frame pulse survives until the nav thread —
                // which ticks on its own cadence — take-and-clears it exactly once (§442, #442).
                // Only overwrite on a fresh landing; otherwise leave any not-yet-consumed value.
                if let Ok(mut v) = self.controller_view.lock() {
                    v.pos = cpos;
                    v.heading = self.heading_target;
                    v.moving = intent.wish_dir[0] != 0.0 || intent.wish_dir[1] != 0.0 || !self.on_ground;
                    // Latch a fresh landing ONLY into an empty view slot, and only TAKE it from the
                    // controller when the slot is free (§442 #442 DEFECT-3 — never drop a real fall's
                    // damage). If the nav thread has not yet consumed a previous landing's height, we
                    // leave the new one in the controller so it is published on a later frame once the
                    // slot frees — the pending fall is applied first, and neither height is clobbered.
                    if v.landed_fall_height.is_none() {
                        if let Some(h) = self.controller.take_landed_fall_height() {
                            v.landed_fall_height = Some(h);
                        }
                    }
                    v.initialized = true;
                }
            }
        }

        // (Removed) The old visual-vs-logical position glide is gone: with a single position
        // authority the controller's position IS the render position, so there is no trailing
        // server position to lerp toward and no key-release snap-back (the rubber-band fix).

        // Vertical physics (gravity, ground clamp, jump, swim) now lives in the CharacterController,
        // integrated in the single-authority movement block above. Nothing to do here.

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
                let motion_deg = crate::coord::eq_heading(de, dn);
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
            // (Nav-driven heading is set from the planner's wish_dir in the movement block above —
            // the render gs heading is no longer kept live by synthetic packets.)
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

        // Egui pass — use associated function to avoid reborrowing self.
        let load_status_text = self.load_status.lock().unwrap().clone();
        let sync_frac = *self.sync_progress.lock().unwrap();
        let prof_egui = crate::profiling::Stopwatch::start();
        let egui_wants_repaint = Self::egui_pass(
            &mut self.egui_state, &mut self.egui_renderer, &self.egui_ctx, &mut self.ui_state, &self.window,
            &mut enc, &view, renderer, self.loading, self.fade, &self.current_zone, &load_status_text,
            sync_frac,
            &self.scene, self.zone_min, self.zone_max,
            self.current_fps, self.zone_map.as_ref(),
            cam_eye, self.collision.as_deref(),
            &self.acts, &self.spells,
            self.show_debug, self.game_state_view.server_corrections,
            &self.frame_profile,
            self.nav_debug,
            self.goto_target.lock().unwrap().map(|(x, y, z)| [x, y, z]),
            self.nav_path_view.lock().unwrap().clone(),
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
                scene:  dur_scene,
                smooth: dur_smooth,
                render: dur_render,
                egui:   dur_egui,
                submit: dur_submit,
                total:  now.elapsed(),
            };
            self.frame_profile.blend(&sample, frame_ms);
        }

        // NOTE: no unconditional `request_redraw()` here. The loop is event-driven — `about_to_wait`
        // decides whether the next frame is needed and only then requests a redraw. A still scene
        // therefore stops rendering and idle CPU drops to ~0. See `about_to_wait`/`wake`.
        // Exception: egui-driven animations (window fades, casting bar, camp countdown easing) have
        // no input/packet to wake the loop, so honor egui's own repaint request (#162).
        if egui_wants_repaint {
            self.wake();
        }
        // GPU borrow (renderer) is released here.
        // pending_reload is checked by window_event after render_frame returns.
    }

    /// Egui render pass. Takes fields as explicit parameters so Rust can verify
    /// they are disjoint from the caller's live `&mut renderer` borrow.
    #[allow(clippy::too_many_arguments)]
    fn egui_pass(
        egui_state:    &mut Option<egui_winit::State>,
        egui_renderer: &mut Option<egui_wgpu::Renderer>,
        egui_ctx:      &Option<egui::Context>,
        ui_state:      &mut crate::ui::UiState,
        window:        &Option<Arc<Window>>,
        encoder:       &mut wgpu::CommandEncoder,
        view:          &wgpu::TextureView,
        renderer:      &EqRenderer,
        loading:       bool,
        fade:          f32,               // zone-transition fade 0..1 (#286)
        current_zone:  &str,
        load_status:   &str,
        sync_progress: Option<f32>,
        scene:         &SceneState,
        zone_min:      [f32; 2],
        zone_max:      [f32; 2],
        current_fps:   f32,
        zone_map:      Option<&zone_map::ZoneMap>,
        cam_eye:       [f32; 3],
        collision:     Option<&collision::Collision>,
        acts:          &crate::ui::Actions,
        spells:        &crate::spells::SpellDb,
        show_debug:    bool,
        corrections:   u32,
        frame_profile: &crate::profiling::FrameProfile,
        nav_debug:     bool,               // navmesh overlay on? (--nav-debug / F11)
        nav_goal:      Option<[f32; 3]>,   // current A* goal for the navmesh overlay
        nav_paths:     (Vec<[f32; 3]>, Vec<[f32; 3]>), // walker's live (coarse, fine) plan (#246)
    ) -> bool {
        let (Some(egui_state), Some(egui_renderer), Some(egui_ctx), Some(window)) =
            (egui_state, egui_renderer, egui_ctx, window) else { return false };

        let raw_input = egui_state.take_egui_input(window);
        let view_proj = renderer.last_view_proj;
        let screen_w  = renderer.surface_config.width;
        let screen_h  = renderer.surface_config.height;

        // Scale the entire UI (text + widgets) with the window: zoom =
        // user_scale × min(w/REF_W, h/REF_H) / dpi — the constraining dimension
        // fits a REF_W×REF_H design canvas exactly, other aspect ratios scale
        // uniformly, and the per-character user multiplier applies on top.
        let nppp = window.scale_factor() as f32;
        let user_scale = ui_state.layout().ui_scale;
        let zoom = ((screen_w as f32 / crate::ui::REF_W)
            .min(screen_h as f32 / crate::ui::REF_H)
            * user_scale
            / nppp)
            .max(0.05);
        egui_ctx.set_zoom_factor(zoom);
        // The TRUE point-space screen size. Never trust ctx.screen_rect() for
        // layout math: set_zoom_factor is applied lazily inside run(), and on
        // the first frame egui's previous screen_rect is a 10000x10000
        // placeholder — remapping/anchoring against it destroys saved layouts.
        let screen_pts = [
            screen_w as f32 / (nppp * zoom),
            screen_h as f32 / (nppp * zoom),
        ];

        let full_output = egui_ctx.run(raw_input, |ctx| {
            // Zone-transition fade backdrop (#286): a full-screen black layer at `fade` alpha, drawn
            // FIRST so the 3D scene (the reposition + the old-then-new zone pop) is hidden behind it
            // while the HUD / loading text render on top and stay legible.
            hud::draw_fade(ctx, fade);
            hud::draw_fps(ctx, current_fps);
            hud::draw_connection_banner(ctx, scene.disconnected);
            // Death overlay + Respawn button for human players (#284): the client no longer
            // auto-respawns, so a human needs a way to revive. Clicking sets the same respawn
            // request POST /v1/lifecycle/respawn drives.
            if hud::draw_death_overlay(ctx, scene.player_dead, &scene.killed_by) {
                acts.command.request_respawn();
            }
            if crate::profiling::enabled() {
                hud::draw_profile(ctx, frame_profile);
            }
            if loading {
                hud::draw_loading(ctx, current_zone, load_status, sync_progress);
            } else {
                hud::draw_labels(ctx, scene, view_proj, screen_w, screen_h, cam_eye, collision);
                if nav_debug {
                    hud::draw_nav_debug(ctx, scene, view_proj, screen_w, screen_h, collision, nav_goal, &nav_paths);
                }
                ui_state.draw_all(ctx, screen_pts, scene, spells, acts, zone_min, zone_max, zone_map, current_fps);
                if show_debug {
                    hud::draw_debug_overlay(ctx, scene.player_pos, scene.player_heading, current_zone, corrections);
                }
            }
        });
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

        // True when egui has an animation in flight (fade, gauge easing, camp
        // countdown): the caller must keep the event-driven loop awake.
        full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map(|v| v.repaint_delay < std::time::Duration::from_millis(200))
            .unwrap_or(false)
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
                // 1024 keeps window text readable in captures (#162); 512 made
                // the new UI's 12pt labels illegible.
                Some(1024),
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
        // Restore the per-character OS window geometry (#162). Size + maximized
        // work everywhere; position restore is best-effort (ignored on Wayland).
        let saved = self.ui_state.layout().os_window;
        let mut attrs = WindowAttributes::default().with_title(&self.window_title);
        let size = saved.map(|s| s.size).unwrap_or([1600, 900]);
        attrs = attrs.with_inner_size(winit::dpi::PhysicalSize::new(size[0].max(320), size[1].max(240)));
        if let Some(st) = saved {
            if let Some([x, y]) = st.pos {
                attrs = attrs.with_position(winit::dpi::PhysicalPosition::new(x, y));
            }
            if st.maximized {
                attrs = attrs.with_maximized(true);
            }
        }
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
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
            // Flush layout on EVERY exit path (POST /exit, GM kick, signals) —
            // CloseRequested already does; this covers the rest (#162).
            self.ui_state.layout_mut().save_now();
            event_loop.exit();
            return;
        }

        // Drain packets + detect in-flight activity. Any activity extends the active render window.
        if self.poll_external() {
            self.active_until = std::time::Instant::now() + Self::ACTIVE_LINGER;
        }

        // Keep rendering while a camp is in progress so the HUD countdown ticks smoothly even in a
        // still scene (the event-driven loop would otherwise idle between sparse packets).
        if self.acts.camp_until.lock().unwrap().is_some() {
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

        // Release events must reach the game even when egui consumes them
        // (typing in chat while holding W): otherwise `keys_held` keeps the key
        // and the character runs forever. Same for losing window focus.
        match &event {
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                if key_event.state == ElementState::Released {
                    if let PhysicalKey::Code(code) = key_event.physical_key {
                        self.keys_held.remove(&code);
                    }
                }
            }
            WindowEvent::Focused(false) => {
                self.keys_held.clear();
                self.drag_active = false;
            }
            _ => {}
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
            WindowEvent::CloseRequested => { self.ui_state.layout_mut().save_now(); event_loop.exit(); }

            WindowEvent::Resized(size) => {
                if let Some((surface, renderer)) = &mut self.gpu {
                    renderer.surface_config.width  = size.width.max(1);
                    renderer.surface_config.height = size.height.max(1);
                    surface.configure(&renderer.device, &renderer.surface_config);
                    renderer.recreate_depth_texture();
                }
                self.record_os_window();
            }

            // Persist the OS window position when the platform reports it
            // (never fires on Wayland; X11/XWayland only).
            WindowEvent::Moved(_) => self.record_os_window(),

            // A pure DPI change (same pixel size) still needs a zoom recompute;
            // the zoom is derived per-frame from window.scale_factor(), so just
            // wake and repaint.
            WindowEvent::ScaleFactorChanged { .. } => {}

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
                                        // ActionLoop::tick (network thread) already polls this same
                                        // slot, sets the real target state, and it flows back via the
                                        // next GameState snapshot — no local echo needed.
                                        self.acts.command.request_target(id);
                                    }
                                    Some(PickResult::Door(door_id)) => {
                                        // Server-authoritative: only request the open; never set is_open
                                        // locally. ActionLoop::tick (network thread) already logs
                                        // "Clicked door {id}" when it polls this same slot.
                                        self.acts.command.request_door_click(door_id);
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
                                        self.acts.command.request_cancel_goto();
                                    }
                                }
                                KeyCode::KeyR | KeyCode::F9 => {
                                    self.camera.reset_to_follow();
                                    self.acts.command.request_cancel_goto();
                                }
                                // Self-target (native EQ F1): target your own character (#291).
                                // Mirrors the click-to-target path — just requests the target;
                                // ActionLoop::tick (network thread) does the real work (OP_TargetMouse +
                                // OP_Consider) and the result flows back via the next GameState snapshot,
                                // enabling self-heals/buffs, consider-on-self, and (server permitting)
                                // GM #kill/#damage on yourself.
                                KeyCode::F1 if !event.repeat => {
                                    let me = self.game_state_view.player_id;
                                    if me != 0 {
                                        self.acts.command.request_target(me);
                                    }
                                }
                                KeyCode::F10 => {
                                    self.show_debug = !self.show_debug;
                                    tracing::info!("DEBUG: overlay {}", if self.show_debug { "ON" } else { "OFF" });
                                }
                                KeyCode::F11 => {
                                    self.nav_debug = !self.nav_debug;
                                    tracing::info!("NAV DEBUG: navmesh overlay {}", if self.nav_debug { "ON" } else { "OFF" });
                                }
                                KeyCode::KeyL
                                    if self.keys_held.contains(&KeyCode::ControlLeft)
                                        || self.keys_held.contains(&KeyCode::ControlRight) =>
                                {
                                    let locked = self.ui_state.layout().locked;
                                    self.ui_state.layout_mut().set_locked(!locked);
                                }
                                // Window toggles route through the registry so
                                // hotkeys live in one table (#162). Ignore OS
                                // key-repeat — holding the key must not strobe
                                // the window open/closed.
                                other if !event.repeat => {
                                    if let Some(key) = winit_to_egui_key(other) {
                                        self.ui_state.hotkey(key);
                                    }
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

/// Map a winit key code to the egui key used by the window registry's hotkeys.
/// Only letters used as window toggles need mapping.
fn winit_to_egui_key(code: KeyCode) -> Option<egui::Key> {
    Some(match code {
        KeyCode::KeyB => egui::Key::B,
        KeyCode::KeyG => egui::Key::G,
        KeyCode::KeyH => egui::Key::H,
        KeyCode::KeyI => egui::Key::I,
        KeyCode::KeyK => egui::Key::K,
        KeyCode::KeyM => egui::Key::M,
        KeyCode::KeyO => egui::Key::O,
        KeyCode::KeyT => egui::Key::T,
        _ => return None,
    })
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

/// Advance the zone-transition fade (#286) one frame toward its target: fully black (1.0) while a
/// zone/position change is `transitioning`, else clear (0.0). Fast to black (`FADE_OUT_S`) so the
/// server-driven reposition + old scene are hidden almost immediately; a slower fade-in (`FADE_IN_S`)
/// reveals the new zone. Pure so the easing is unit-testable off the render loop.
fn next_fade(current: f32, transitioning: bool, dt: f32) -> f32 {
    const FADE_OUT_S: f32 = 0.12; // clear → black
    const FADE_IN_S:  f32 = 0.40; // black → clear
    let target = if transitioning { 1.0 } else { 0.0 };
    if current < target {
        (current + dt / FADE_OUT_S).min(target)
    } else if current > target {
        (current - dt / FADE_IN_S).max(target)
    } else {
        current
    }
}

/// Distance (units from the player) within which entity billboards get per-frame motion smoothing
/// (dead-reckoned gliding). Same rationale as [`crate::renderer::ANIM_ADVANCE_DIST`] (#152,
/// PR #161): the skinned entity pass culls everything past [`crate::pass::ENTITY_DRAW_DIST`], so
/// gliding a farther entity a fraction of a unit per frame is pure CPU with zero on-screen effect —
/// in a crowded outdoor zone (~700 spawns) that work dominated the update phase. MUST be ≥
/// `ENTITY_DRAW_DIST` (margin included) so no entity is ever DRAWN un-smoothed; see the invariant
/// test below. The floor snap is NOT gated by this — see [`smooth_entity_motion`].
pub(crate) const MOTION_SMOOTH_DIST: f32 = crate::pass::ENTITY_DRAW_DIST + 48.0;

/// Smooth NPC movement (entities within [`MOTION_SMOOTH_DIST`] of the player only) and snap ALL
/// billboards to the terrain floor (memoized, so it's ~free for anything not actively moving).
///
/// Server position updates (OP_ClientUpdate) arrive only a few times per second, so snapping each
/// billboard to the latest packet looks choppy. Instead we estimate each entity's velocity from its
/// last two server positions and dead-reckon it forward, so it travels continuously at its actual
/// pace. Large horizontal jumps (spawns, teleports, server corrections) snap instead of sliding.
/// The floor snap runs on the smoothed position so the ground height follows the glide.
///
/// Entities beyond the gate track the raw server position (display == target, speed 0): their
/// skinned model isn't drawn out there, so per-frame gliding would be invisible CPU burn — but the
/// billboard footprints that DO still render at any distance (name label, fallback quad for
/// model-less races, minimap dot) must stay grounded exactly as before #152, which the shared
/// memoized floor snap provides at ~zero cost (a far entity re-raycasts only when a sparse server
/// update actually moves it, not per frame). Because display tracks the raw position while far, an
/// entity re-entering the gate starts from its current server pos and SNAPS there instead of
/// gliding across the distance it covered while out of range.
fn smooth_entity_motion(
    motion:     &mut std::collections::HashMap<u32, EntityMotion>,
    billboards: &mut [crate::scene::Billboard],
    player_pos: [f32; 3],
    collision:  Option<&crate::nav::collision::Collision>,
    now:        std::time::Instant,
    dt:         f32,
) {
    // Snap (jump instead of slide) only on an implausibly fast jump — a real teleport /
    // server correction — judged by the IMPLIED speed, not raw distance. RoF2 streams NPC
    // positions sparsely and irregularly, so ordinary movement routinely covers 25-90+
    // units between updates (measured in neriakc: median ~10 u/s, p99 ~19 u/s, essentially
    // all < 100 u/s). The old 25-unit distance cutoff snapped ~23% of real moves into
    // visible instant lurches; keying off implied speed lets those slide while still
    // snapping genuine teleports (>TELEPORT_SPEED). (eqoxide#1)
    const TELEPORT_SPEED: f32 = 100.0;     // u/s; above this an update is a teleport, not motion
    const MAX_UPD: f32 = 4.0;              // cap on the measured update interval. RoF2 NPCs
                                           // send a position only ~every 2.7s; the old 1.0s
                                           // cap made the pace estimate ~3x too high, so the
                                           // entity lurched to each point then waited.
    // Ids alive this frame. Everything else's motion state is dropped below, so despawned
    // entities don't leak state.
    let mut live: std::collections::HashSet<u32> = std::collections::HashSet::new();

    for b in &mut *billboards {
        let target = b.pos;
        live.insert(b.id);
        let m = motion.entry(b.id).or_insert_with(|| EntityMotion {
            display: target, target, speed: 0.0, last_update: now,
            floor_at: [f32::NAN; 3], floor_z: 0.0,
        });

        let (dx, dy, dz) = (target[0] - player_pos[0],
                            target[1] - player_pos[1],
                            target[2] - player_pos[2]);
        if dx * dx + dy * dy + dz * dz > MOTION_SMOOTH_DIST * MOTION_SMOOTH_DIST {
            // Beyond the smoothing gate: skip the per-frame glide (the skinned model isn't drawn
            // past ENTITY_DRAW_DIST, so gliding would be invisible CPU burn) and track the raw
            // server position instead, so the shared floor snap below keeps the still-rendered
            // footprints (label / fallback quad / minimap dot) grounded and a re-entering entity
            // snaps rather than gliding on stale state. `last_update` advances only on a real
            // position change, keeping the pace estimate honest for the first move after re-entry.
            if target != m.target {
                m.target = target;
                m.last_update = now;
            }
            m.display = target;
            m.speed = 0.0;
        } else {
            // A changed server position is a fresh update: estimate the travel pace from the
            // distance moved since the previous one over the real elapsed interval.
            if target != m.target {
                let dx = target[0] - m.target[0];
                let dy = target[1] - m.target[1];
                let dz = target[2] - m.target[2];
                let dt_upd = (now - m.last_update).as_secs_f32().clamp(0.05, MAX_UPD);
                let horiz = (dx * dx + dy * dy).sqrt();
                if horiz / dt_upd > TELEPORT_SPEED {
                    m.speed = 0.0;          // teleport / correction — snap, don't slide across
                    m.display = target;
                } else {
                    m.speed = (horiz * horiz + dz * dz).sqrt() / dt_upd;
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
            if b.action == "idle" {
                // Swim animation for an NPC/PC in water (#198/#207), same water check the player
                // uses: the active stroke while moving, treading water when holding still, so a
                // character in water never appears to stand on the surface. Walking on dry land;
                // still on dry land stays idle.
                let in_water = collision.is_some_and(|c| c.in_water(b.pos));
                let moving = m.speed > 0.5 && d > 1e-4;
                if in_water {
                    b.action = if moving { "swimming" } else { "treading" }.to_string();
                } else if moving {
                    b.action = "walking".to_string();
                }
            }

            // Face the direction of travel while moving, exactly like the player does. The
            // server `heading` field is stale between the sparse position updates and often
            // points ~180° from the glide vector, so rendering it verbatim made moving NPCs
            // appear to walk backwards. Derive heading (degrees, 0=north) from the glide delta
            // `to` (east=to[0], north=to[1]); when stopped, keep the authoritative server
            // heading (b.heading is refreshed from the entity each frame). (eqoxide#106)
            if d > 0.1 && m.speed > 0.5 {
                b.heading = crate::coord::eq_heading(to[0], to[1]);
            }
        }

        // Snap the billboard to the terrain floor so it doesn't hover above geometry.
        // NPCs get z from the server spawn/update packets; the player gets floor_z
        // applied each frame. Same grounding here, on the smoothed position — for ALL
        // entities (labels / fallback quads / minimap dots render at any distance), but
        // memoized: the downward raycast is the single most expensive piece of the old
        // every-entity loop, and the compared position is bit-identical frame to frame
        // unless the entity actually moved (near: the glide has settled; far: the raw
        // server pos only changes on a sparse update), so only re-raycast on movement (#152).
        if b.floating {
            // Boats/ships float on the water surface: keep their server-sent z, do NOT snap to the
            // floor. The server skips FixZ for boats too (Mob::FixZ: `if (GetIsBoat()) return;`)
            // because they're GravityBehavior::Floating; floor_z would find the seabed/dock a few
            // units down in shallow harbor water and yank the ship underwater (#194).
        } else {
            match collision {
                Some(col) => {
                    if b.pos != m.floor_at {
                        m.floor_at = b.pos;
                        m.floor_z  = col.floor_z(b.pos[0], b.pos[1], b.pos[2]);
                    }
                    b.pos[2] = m.floor_z;
                }
                // No collision loaded (zone (re)loading): invalidate the cache so the snap is
                // recomputed against the NEW zone geometry once it arrives, not served stale.
                None => m.floor_at = [f32::NAN; 3],
            }
        }
    }

    motion.retain(|id, _| live.contains(id));
}

#[cfg(test)]
mod tests {
    use super::{smooth_entity_motion, zone_needs_reload, next_fade, EntityMotion, MOTION_SMOOTH_DIST};
    use std::collections::HashMap;

    fn bb(id: u32, pos: [f32; 3]) -> crate::scene::Billboard {
        crate::scene::Billboard {
            id, pos,
            level: 1, hp_pct: 100.0, is_target: false, dead: false,
            name: format!("npc{id}"), race: "HUM".into(), action: "idle".into(),
            heading: 0.0, equipment: [0; 9], equipment_tint: [[0; 3]; 9],
            gender: 0, face: 0, hairstyle: 0, haircolor: 0, helm: 0, showhelm: 0, floating: false,
        }
    }

    /// Flat floor at z=`h` spanning east/north [-100,100], for floor-snap tests.
    fn flat_collision_at(h: f32) -> crate::nav::collision::Collision {
        use crate::assets::{MeshData, RenderMode, ZoneAssets};
        use crate::nav::collision::Collision;
        let floor = MeshData {
            positions: vec![[-100.0, h, -100.0], [100.0, h, -100.0],
                            [100.0, h, 100.0], [-100.0, h, 100.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 8.0)
    }

    // ── #152: per-entity motion smoothing / floor snap is distance-gated ─────────────────────

    /// INVARIANT: the smoothing gate must cover the draw distance. If an entity can be DRAWN
    /// (within ENTITY_DRAW_DIST of the player) it MUST be smoothed and floor-snapped, or it
    /// would visibly jitter between sparse server updates / hover above the ground. Mirrors
    /// the ANIM_ADVANCE_DIST invariant from PR #161.
    #[test]
    fn motion_gate_covers_draw_distance() {
        assert!(MOTION_SMOOTH_DIST >= crate::pass::ENTITY_DRAW_DIST,
            "motion gate {MOTION_SMOOTH_DIST} must be >= draw cull {}", crate::pass::ENTITY_DRAW_DIST);
    }

    #[test]
    fn distant_entity_is_not_glided_and_despawn_drops_state() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let now = std::time::Instant::now();
        let far = [MOTION_SMOOTH_DIST + 100.0, 0.0, 0.0];
        // Two ticks: an out-of-range entity's raw server position passes through untouched
        // (no glide state ever forms — display tracks the raw pos exactly, speed stays 0).
        let mut bbs = vec![bb(7, far)];
        for _ in 0..2 {
            smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, now, 1.0 / 60.0);
        }
        assert_eq!(bbs[0].pos, far, "distant entity keeps its raw server position");
        assert_eq!(motion[&7].display, far, "display must track the raw pos while out of range");
        assert_eq!(motion[&7].speed, 0.0, "no glide pace may accumulate while out of range");
        // Despawn (entity absent this frame) → its state is dropped, no leak.
        smooth_entity_motion(&mut motion, &mut [], [0.0; 3], None, now, 1.0 / 60.0);
        assert!(motion.is_empty(), "despawned entity's motion state must be dropped");
    }

    #[test]
    fn near_entity_glides_toward_moved_target() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let t0 = std::time::Instant::now();
        // Frame 1: entity appears at origin-ish → seeds state at the server pos.
        let mut bbs = vec![bb(7, [10.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t0, 1.0 / 60.0);
        assert_eq!(bbs[0].pos, [10.0, 0.0, 0.0], "first sight snaps to the server position");
        // Frame 2 (~1s later): server pos moved 10u east → implied speed ~10u/s, so after a
        // 1/60s tick the display must have moved a fraction of the way, not jumped.
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb(7, [20.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        let x = bbs[0].pos[0];
        assert!(x > 10.0 && x < 12.0, "expected a small glide step from 10 toward 20, got {x}");
        assert_eq!(bbs[0].action, "walking", "gliding entity overrides idle with walking");
    }

    #[test]
    fn reentering_entity_snaps_instead_of_gliding_stale_state() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let t0 = std::time::Instant::now();
        // Seed near state, mid-glide (display lags target).
        let mut bbs = vec![bb(7, [10.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t0, 1.0 / 60.0);
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb(7, [20.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert!(bbs[0].pos[0] < 20.0, "precondition: display lags the target mid-glide");
        // Entity leaves range for a frame → its display must jump to tracking the raw pos …
        let far = [MOTION_SMOOTH_DIST + 100.0, 0.0, 0.0];
        let mut bbs = vec![bb(7, far)];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert_eq!(motion[&7].display, far, "out-of-range entity's display tracks the raw pos");
        // … so on re-entry it snaps to the fresh server position instead of gliding
        // from the stale display across the distance covered while out of range.
        let mut bbs = vec![bb(7, [30.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert_eq!(bbs[0].pos, [30.0, 0.0, 0.0], "re-entering entity snaps to the server position");
    }

    #[test]
    fn near_entity_floor_snaps_and_memoizes() {
        let col_a = flat_collision_at(0.0);
        let col_b = flat_collision_at(2.0); // different height — any re-raycast is detectable
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let now = std::time::Instant::now();
        // Frame 1: entity hovering at z=5 over the z=0 floor → raycast, snapped to z=0.
        let mut bbs = vec![bb(7, [10.0, 0.0, 5.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col_a), now, 1.0 / 60.0);
        assert!(bbs[0].pos[2].abs() < 1e-3, "hovering entity snaps to floor, got z={}", bbs[0].pos[2]);
        // Frames 2-3: SAME position, but the floor swapped to z=2. A working memo cache serves
        // the stored z=0 WITHOUT re-raycasting; a silently broken cache would re-raycast and
        // return z=2 — so this pins that the raycast really ran only once.
        for _ in 0..2 {
            let mut bbs = vec![bb(7, [10.0, 0.0, 5.0])];
            smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col_b), now, 1.0 / 60.0);
            assert!(bbs[0].pos[2].abs() < 1e-3,
                "stationary entity must be served from the memo cache (no re-raycast), got z={}",
                bbs[0].pos[2]);
        }
        // Server moves the entity → cache invalidated → fresh raycast against the CURRENT
        // floor (z=2). Guards against a cache that never invalidates.
        let mut bbs = vec![bb(7, [50.0, 0.0, 5.0])]; // 40u jump in one tick = teleport snap
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col_b), now, 1.0 / 60.0);
        assert!((bbs[0].pos[2] - 2.0).abs() < 1e-3,
            "moved entity must re-raycast against the current floor, got z={}", bbs[0].pos[2]);
    }

    #[test]
    fn distant_entity_is_still_floor_snapped_for_labels() {
        // A far entity's skinned model isn't drawn, but its name label / fallback quad /
        // minimap dot still render at any distance — so it must stay grounded exactly as
        // before the #152 gate (memoized: re-raycast only when the server pos changes).
        let col = flat_collision_at(0.0);
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let now = std::time::Instant::now();
        let player = [1000.0, 0.0, 0.0]; // entity is ~990u away — well past MOTION_SMOOTH_DIST
        assert!(1000.0 - 10.0 > MOTION_SMOOTH_DIST, "precondition: entity is out of range");
        for _ in 0..2 {
            let mut bbs = vec![bb(8, [10.0, 0.0, 5.0])];
            smooth_entity_motion(&mut motion, &mut bbs, player, Some(&col), now, 1.0 / 60.0);
            assert!(bbs[0].pos[2].abs() < 1e-3,
                "distant entity's billboard must snap to the floor, got z={}", bbs[0].pos[2]);
        }
    }

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
    fn zone_fade_blacks_out_fast_and_fades_in_slower() {
        // #286: entering a transition ramps to fully black quickly (~0.12s), then holds; leaving it
        // fades back to clear more slowly (~0.4s). Both directions clamp to [0,1] and never overshoot.
        // Fade to black: from clear, ~0.12s of 60fps steps reaches ~1.0.
        let mut f = 0.0;
        for _ in 0..8 { f = next_fade(f, true, 1.0 / 60.0); } // ~0.133s
        assert!(f >= 0.999, "should be fully black after ~0.13s transitioning, got {f}");
        // Holds at black while still transitioning.
        assert_eq!(next_fade(1.0, true, 1.0 / 60.0), 1.0);
        // Fade in: from black, ~0.12s in should still be partly dark (slower than the fade-out).
        let mut g = 1.0;
        for _ in 0..8 { g = next_fade(g, false, 1.0 / 60.0); }
        assert!(g > 0.5, "fade-in is slower than fade-out; still dark after ~0.13s, got {g}");
        // Eventually reaches clear and clamps (no negative).
        let mut h = 0.05;
        for _ in 0..8 { h = next_fade(h, false, 1.0 / 60.0); }
        assert_eq!(h, 0.0, "fade-in clamps to clear, got {h}");
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

/// Clear `door_frac` if the game state's zone has already moved on from `current_zone`. Door ids
/// are per-zone `u8`s that collide across zones (door_id=3 in zone A and door_id=3 in zone B are
/// unrelated doors), so a stale entry left over from the old zone must not survive into the new
/// one. Extracted as a pure, testable step (#326): the caller MUST run this before `door_frac` is
/// read to seed/ease the frame's doors, or that frame's scene is built from the old zone's
/// fraction for one frame — the new zone's door flashes at the previous zone's open/closed state
/// before snapping shut/open on the following frame.
fn reset_door_frac_on_zone_change(
    door_frac: &mut std::collections::HashMap<u8, f32>,
    incoming_zone: &str,
    current_zone: &str,
) {
    if zone_needs_reload(incoming_zone, current_zone) {
        door_frac.clear();
    }
}

#[cfg(test)]
mod reset_door_frac_tests {
    use super::*;

    #[test]
    fn clears_when_zone_changed() {
        let mut door_frac = std::collections::HashMap::new();
        door_frac.insert(3u8, 1.0f32); // door_id=3 left open in the old zone
        reset_door_frac_on_zone_change(&mut door_frac, "gfaydark", "qeynos");
        assert!(door_frac.is_empty(), "stale fraction must not survive a zone change");
    }

    #[test]
    fn leaves_map_untouched_when_zone_unchanged() {
        let mut door_frac = std::collections::HashMap::new();
        door_frac.insert(3u8, 0.42f32);
        reset_door_frac_on_zone_change(&mut door_frac, "qeynos", "qeynos");
        assert_eq!(door_frac.get(&3u8).copied(), Some(0.42f32));
    }

    #[test]
    fn leaves_map_untouched_when_incoming_zone_empty() {
        // Matches `zone_needs_reload`'s own guard: an empty zone name means "not loaded yet",
        // not a real zone change.
        let mut door_frac = std::collections::HashMap::new();
        door_frac.insert(3u8, 0.42f32);
        reset_door_frac_on_zone_change(&mut door_frac, "", "qeynos");
        assert_eq!(door_frac.get(&3u8).copied(), Some(0.42f32));
    }
}

/// Seconds for a door to fully swing/slide from closed to open (or back).
const DOOR_TRAVEL_SECS: f32 = 0.5;

/// One easing step for a door's render-only open fraction, moving `current` toward the target
/// implied by `is_open` proportionally (an exponential ease with time-constant governed by
/// `full_travel_secs`), matching the old in-`GameState` tween exactly. Snaps exactly to the
/// target once within 0.001 of it.
fn ease_door_frac(current: f32, is_open: bool, dt: f32, full_travel_secs: f32) -> f32 {
    let target = if is_open { 1.0_f32 } else { 0.0_f32 };
    let step = (dt / full_travel_secs).min(1.0);
    let next = current + (target - current) * step;
    if (next - target).abs() < 0.001 { target } else { next }
}

#[cfg(test)]
mod door_frac_tests {
    use super::*;

    #[test]
    fn eases_toward_open_target_and_snaps_on_arrival() {
        let frac = ease_door_frac(0.0, true, 0.25, 0.5);
        assert!((frac - 0.5).abs() < 1e-6);
        let frac = ease_door_frac(frac, true, 0.5, 0.5); // a full extra travel-window's worth of dt
        assert_eq!(frac, 1.0);
    }

    #[test]
    fn eases_toward_closed_target() {
        let frac = ease_door_frac(1.0, false, 0.25, 0.5);
        assert!((frac - 0.5).abs() < 1e-6);
    }

    #[test]
    fn dt_larger_than_full_travel_snaps_immediately() {
        let frac = ease_door_frac(0.0, true, 10.0, 0.5);
        assert_eq!(frac, 1.0);
    }
}
