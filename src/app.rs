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
use crate::camera_state::{lerp_angle, CameraCmd, CameraSnapshot, CameraState};
use crate::frame_capture::encode_frame_png;
use crate::game_state::GameState;

use crate::ipc::FrameReq;
use crate::renderer::EqRenderer;
use crate::scene::SceneState;
use crate::nav::collision;
use crate::{assets, debug_zone, hud, zone_map};

/// Data produced by the background zone-load thread, ready for GPU upload on the main thread.
struct PendingLoad {
    /// Monotonic id of the load that produced this result (#595 review F3), so a slow OLD loader
    /// cannot overwrite a NEWER one's reply in the single handoff slot.
    gen: u64,
    zone_name: String,
    /// None means the S3D failed to load; use the fallback ground plane instead.
    assets:    Option<assets::ZoneAssets>,
    /// Why `assets` is None, verbatim from the loader (#579). Carried through so the observable
    /// `zone_assets` state can report a real FAILED reason instead of an eternal "pending".
    load_error: Option<String>,
    collision: Option<Arc<collision::Collision>>,
    /// The HUD minimap's map-load OUTCOME, carried whole (#873, #877 rounds 2–3) — see
    /// [`zone_map::ZoneMapLoad`] for why it is a newtype and not an
    /// `Option<Result<ZoneMap, ZoneMapLoadError>>`, and for the precise bound of what that buys.
    ///
    /// Short version, because two earlier rounds each overstated this: the map and its failure
    /// reason are ONE value so a call site cannot keep the first and drop the second, and the
    /// newtype's private field means the "keep the map, drop the reason" shape — `#873` itself —
    /// does not compile outside `zone_map`. Round 2 claimed the bare `Option<Result<..>>` already
    /// achieved that; the round-2 reviewer measured otherwise (`try_load(..).ok().map(Ok)` compiled,
    /// and the whole workspace stayed green), which is why the newtype exists.
    zone_map: zone_map::ZoneMapLoad,
    zone_min:  [f32; 2],
    zone_max:  [f32; 2],
}

/// #873: render a `ZoneMap` load outcome into what the HUD minimap needs — the map to draw, if any,
/// and the reason there isn't one, if that absence is a DEFECT. Both halves come out of one call, so
/// a caller that wants only the map has to write the discard of the reason explicitly rather than
/// getting it from a bare `.ok()` (see [`zone_map::ZoneMapLoad`] for what the input type does and
/// does not make impossible — this function's signature is a convenience, not a guarantee).
///
/// **`Missing` is deliberately NOT a reason** (#877 round 2, owner direction). `Missing` and
/// `LayerUnreadable` are different events and must not render identically: a zone that ships no
/// `.txt` map at all is an ordinary, expected state — measured against the shipped map pack, 27
/// zones that ship a `.wtr` have no base `.txt`, including `tutorial`, `arena2`, `bazaar_v0`/`_v1`,
/// `guildhall3`, `shadowedmount`, `nektulos_v0`/`_v1` and `load`/`load2` — and painting "map data
/// unavailable: …" over those would report a non-failure as a failure. That is the agent-honesty
/// invariant pointing the other way: a false alarm is as dishonest as a false success, and an alarm
/// that fires on 27-plus ordinary zones is exactly how a real one stops being read. (27 is a FLOOR,
/// not a total: it is exact only for the 497 zones that ship a `water/<zone>.wtr`, which is the set
/// the measurement enumerated — zones shipping neither file were never counted.) The two
/// present-but-broken causes (`Unreadable`, `LayerUnreadable`) ARE defects a driver should see, so
/// only those carry a reason.
///
/// This filter is HUD-only. `/v1/observe/debug`'s `zone_map_load` still reports all three causes
/// distinctly (see `eqoxide-net`'s `sync_zone_points`) — the agent-facing disclosure is unchanged.
fn hud_zone_map_view(
    outcome: Option<&Result<zone_map::ZoneMap, zone_map::ZoneMapLoadError>>,
) -> (Option<&zone_map::ZoneMap>, Option<String>) {
    match outcome {
        None => (None, None),
        Some(Ok(zm)) => (Some(zm), None),
        Some(Err(zone_map::ZoneMapLoadError::Missing)) => (None, None),
        Some(Err(e)) => (None, Some(e.to_string())),
    }
}

/// Hand a finished load to the main thread, refusing to displace a NEWER load's result (#595
/// review F3). The handoff is a SINGLE slot shared by every loader, so an unconditional write let a
/// slow OLD loader clobber a newer zone's already-published reply; the main thread then dropped the
/// stale one on its zone check and nothing ever arrived for the zone the character was in — an
/// eternal `Pending`.
fn publish_load(slot: &Arc<Mutex<Option<PendingLoad>>>, gen: u64, load: PendingLoad) {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    match slot.as_ref() {
        Some(existing) if existing.gen > gen => tracing::warn!(
            "APP: load #{gen} ('{}') finished after newer load #{} — discarding it rather than \
             overwriting the newer result", load.zone_name, existing.gen),
        _ => *slot = Some(load),
    }
}

/// **The client's ONE production construction of a zone's collision grid**: build it from the
/// terrain, then attach the zone's region map (`<maps_dir>/water/<zone>.wtr`) — water volumes for
/// swim/descend routing, AND the DRNTP zone-line regions that ARE this zone's exits.
///
/// #803: the loader's `Err` is kept and installed on the grid. A discarded failure used to read as
/// "this zone has no water and no exits", which `/v1/observe/zone_exits` published as `[]` with 200
/// OK — a confident "there is no way out of here" that the agent had no way to doubt.
///
/// **Why this is a named function and not four lines inside the loader closure** (#821 review round
/// 2, N2): while it lived inline in a thread closure it could not be called from a test, and the
/// reviewer proved the consequence by deleting the `set_region_data` line — the whole root crate
/// stayed green, though every zone's grid would then carry `Err(NotAttached)` and `zone_exits` would
/// 503 in **every zone in the game**. The type change in #803 forced all the *library* call sites to
/// speak up; the line that actually feeds them in the shipped client was pinned by nothing. It is
/// pinned by `zone_load_wiring_803` below.
///
/// One deliberate behaviour change from the inline version: the `.wtr` load (and its `warn!`) now
/// happens only when the terrain assets loaded. When they did not, no grid is built at all and the
/// zone publishes `Failed` — a warning about the region data of a world that does not exist was
/// noise, and there is no grid for it to be about.
pub(crate) fn build_zone_collision(
    za: &assets::ZoneAssets,
    maps_dir: &std::path::Path,
    zone_name: &str,
) -> collision::Collision {
    let water = crate::region_map::RegionMap::try_load(&maps_dir.join("water"), zone_name)
        .map(Arc::new);
    if let Err(e) = &water {
        tracing::warn!("region_map: zone '{}' has no usable region data: {}", zone_name, e);
    }
    let mut c = collision::Collision::build(za, 32.0);
    c.set_region_data(water);
    c
}

/// The `watch_for_lost_load` decision, pure so it can be tested (#595 review F3). `Some(zone)` means
/// the state is stuck `Pending` for `zone` and no loader is left that could ever report it — a
/// panic, or a reply clobbered in the handoff slot. `None` means leave it alone: either a loader is
/// still working (however slow) or the state is already terminal.
///
/// **Why the arms are written out instead of `_ => None`** (#838). This is the decision function for
/// [`App::watch_for_lost_load`], which is the only detector for a loader that died without
/// reporting; when it says `None` the watchdog returns early and nothing else re-examines the state.
/// A wildcard is *correct* for every state that exists today — `Idle`, `Ready` and `Failed` should
/// all be left alone — and that is exactly what made it dangerous: it reads as deliberate, and it
/// would go on silently answering `None` for a **fifth, in-flight** variant added later. That
/// variant would then never be declared lost, and an agent polling a frozen in-flight state cannot
/// tell "still loading, be patient" from "nothing is coming, ever" — the same lie the module header
/// of `zone_assets` says `Failed` exists to prevent.
///
/// What the explicit arms buy, stated no wider than it is: adding a variant to
/// [`crate::nav::zone_assets::ZoneAssetState`] makes **this file fail to compile** with `E0004`,
/// which forces whoever adds it to decide, at this site, whether the new state is in-flight or
/// terminal. Measured on the #838 PR by adding a fifth in-flight variant to that enum — but *which
/// invocation* reds this file is load-bearing, and most of them do not. `cargo check -p eqoxide-nav`
/// does **not** red this file: the same variant reds `eqoxide-nav`'s own lib with six `E0004`s (all
/// in `zone_assets.rs`), compilation stops upstream, and the crate holding this file is never built
/// at all — `Checking eqoxide v` appears **0** times in that run. Only once those six nav arms are
/// filled in (`=> todo!()` suffices) does `cargo check -p eqoxide --locked` reach this crate, and it
/// then reports **exactly one** `E0004` in package `eqoxide`, at `src/app.rs:136:11`, which is this
/// `match`; adding `--all-targets` to that same `-p eqoxide` check changes neither number. With
/// `_ => None` put back and the probe variant and filled nav arms still in place, that same command
/// exits 0 with zero errors and zero warnings, `Checking eqoxide v0.1.0` present once as the reach
/// control. (Package `eqoxide-nav` checked with `--all-targets` reports one further `E0004`, at
/// `zone_assets.rs:705`, inside #826's own test — a different package, not part of the count above.)
/// What that does *not* buy: it does not stop someone re-introducing a
/// wildcard later, and it does not make the new arm's answer correct — only forced. No runtime test
/// can close either gap, because a test can only construct variants that already exist, so nothing
/// below can tell the two forms apart. The compiler is the whole guard. Do not collapse these arms
/// back into `_`.
///
/// Same class of fix as #826/#837 on `ZoneAssetState::collision()`, which lives in
/// `crates/eqoxide-nav/`. Its `E0004` probe did not name this site — not because a wildcard
/// absorbed the variant, but because a probe scoped to `eqoxide-nav` never compiles this crate at
/// all (see the measurement above).
fn lost_load_zone(any_loader_alive: bool, st: &crate::nav::zone_assets::ZoneAssetState) -> Option<String> {
    use crate::nav::zone_assets::ZoneAssetState as S;
    if any_loader_alive { return None; }
    match st {
        // In flight, with nothing left that could ever report it — declare it lost.
        S::Pending { zone, .. } => Some(zone.clone()),
        // Nothing is on its way, so there is no load to declare lost.
        S::Idle => None,
        // Terminal states: already answered, never re-declared lost.
        S::Ready { .. } => None,
        S::Failed { .. } => None,
    }
}

/// The two ways the common-asset-loader thread can end in failure (#616 review F2). They read alike
/// (both are `Err(String)`-shaped) but call for OPPOSITE `poll_sync` treatment, so they must be told
/// apart rather than folded into one `Err(String)`:
///
/// - `Ordinary` — the loader's body ran to completion and reached a state with no usable asset set
///   (sync failed and no cached fallback exists). This predates #616 and `poll_sync` has always held
///   the loading screen up FOREVER for it — `self.loading` stays `true`, the error stays on screen,
///   and the client deliberately never proceeds into a broken game with no character models. That is
///   a real, actionable, LOUD block, not a silent degrade, and #616 does not touch it.
/// - `Panicked` — the wrapper's `catch_unwind` caught an unwind; the body never finished, so there is
///   nothing more for the loading screen to usefully hold open on. This is the NEW case #616 adds:
///   `poll_sync` clears `loading` and hands the reason to the persistent `common_assets_failed`
///   field instead (see its doc on `App`) so the failure is not lost the instant the loading screen
///   stops drawing.
///
/// The #616 review caught an EARLIER version of this fix that used a bare `Result<(), String>` for
/// both and unconditionally cleared `loading` in `poll_sync`'s `Err` arm — which silently reintroduced
/// the very "proceed as if fine" bug #616 exists to remove, one layer up, for the `Ordinary` case.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LoaderFailure {
    Ordinary(String),
    Panicked(String),
}

/// The pure decision behind `poll_sync`'s `Err` arm (#616 review F2): the message to publish (to
/// both the transient loading-screen text and the persistent `common_assets_failed` observable,
/// which are the same for both variants) and whether `self.loading` should be cleared (which is NOT
/// the same for both — see `LoaderFailure`'s doc). Extracted, like `lost_load_zone` /
/// `publish_load` above, so the panic-vs-ordinary distinction is directly unit-testable without
/// constructing a full GPU-backed `App`.
fn common_asset_loader_failure_outcome(f: LoaderFailure) -> (String, bool) {
    match f {
        LoaderFailure::Panicked(msg) => (msg, true),
        LoaderFailure::Ordinary(msg) => (msg, false),
    }
}

/// Runs the common-asset-loader thread body under `catch_unwind`; if it panics, publishes an
/// explicit `Err(LoaderFailure::Panicked(_))` — see `LoaderFailure` for why that variant, not
/// `Ordinary`, matters (#616 review F2). Before this wrapper, `done` was written only by `body`'s
/// own last line, so a panic anywhere above it (a corrupt manifest, an I/O panic mid-reassembly)
/// left `done` `None` forever: `poll_sync` never sees a result, `self.loading` never clears, and the
/// loading screen (showing whatever status text was last set before the panic) is frozen for the
/// rest of the session — the exact "waiting for a result that can never arrive" #579/#595 exist to
/// prevent, just via a different worker. Mirrors the zone-asset loader's `catch_unwind` (src/app.rs,
/// added by #595) rather than inventing a new shape.
fn run_common_asset_loader<F>(body: F, done: &Arc<Mutex<Option<Result<(), LoaderFailure>>>>)
where
    F: FnOnce() + std::panic::UnwindSafe,
{
    if std::panic::catch_unwind(body).is_err() {
        let reason = "the common-asset-loader thread PANICKED while syncing assets (see the \
                       crash log). No retry is running.".to_string();
        tracing::error!("APP: common-asset-loader thread panicked");
        *done.lock().unwrap_or_else(|e| e.into_inner()) = Some(Err(LoaderFailure::Panicked(reason)));
    }
}

/// Runs the model-sync-worker thread body under `catch_unwind`, always leaving `dead` with the
/// reason the worker stopped — success is impossible for this worker (it only stops by dying), so
/// `body` returns why (#616). Before this wrapper a panic just ended the thread with NOTHING
/// published: the renderer keeps sending on-demand race-model requests down a channel whose
/// receiver is now gone, `let _ = tx.send(..)` silently swallows the resulting error, and on-demand
/// character-model syncing never happens again for the rest of the session with no signal anywhere
/// that it died — model syncing degrades silently instead of failing loud.
fn run_model_sync_worker<F>(body: F, dead: &Arc<Mutex<Option<String>>>)
where
    F: FnOnce() -> String + std::panic::UnwindSafe,
{
    let reason = match std::panic::catch_unwind(body) {
        Ok(reason) => reason,
        Err(_) => {
            tracing::error!("APP: model-sync-worker thread panicked");
            "the model-sync-worker thread PANICKED — on-demand race-model syncing will not \
             happen again this session (see the crash log). No retry is running.".to_string()
        }
    };
    *dead.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
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

/// The surface-retry back-off's **entire** state, and every transition on it (#895 review B1).
///
/// Exists because "pending work keeps the loop awake" and "the surface never hands out a texture"
/// compose badly: the pending signal is never consumed, so the loop re-arms at `FRAME_INTERVAL` and
/// re-enters a `render_frame` that reconfigures the swapchain and returns, indefinitely. That
/// predates #895 (`frame_req` behaves the same way, and is why a take-below-the-match fix does not
/// create the class), but #895 adds `camera_cmd` to the set of signals that can hold it, so this
/// bounds the cost rather than leaving it unnamed.
///
/// **Why a struct and not a bare `u32` field on `App`.** `App` cannot be constructed in a unit test
/// — it owns a window, a wgpu surface, a tokio handle and a dozen channels — so while the streak was
/// an `App` field, *nothing that moved it was reachable from a test*. Round-2 review measured that:
/// wrapping the increment in `if std::hint::black_box(false) { .. }` left the crate's lib tests at
/// `249 passed; 0 failed` and left `cargo check` completely silent, and the same held for
/// `wake()`'s clear. This type is constructible, so
/// [`the_state_machine_backs_off_after_a_failure_run_and_recovers_895`] drives the real transitions
/// on the real field and watches the interval change.
///
/// **What is pinned and what is still only read** — stated precisely, because the sentence this
/// replaces claimed the compiler enforced things it does not:
///
/// | site | mutation | caught by |
/// | --- | --- | --- |
/// | the assignment inside [`fold`](Self::fold) | delete **or** wrap | the state-machine test |
/// | the assignment inside [`note_window_event`](Self::note_window_event) | delete **or** wrap | the state-machine test |
/// | the threshold arm reached via [`wake_interval`](Self::wake_interval) | delete **or** wrap | the sweep + state-machine tests |
/// | `render_frame`'s call to `fold` | **wrap** in `if .. { }` | **the compiler** — `output` has no other source, so the wrap does not build |
/// | `render_frame`'s call to `fold` | *unwrap* it — call `surface.get_current_texture()` straight | **nothing** — reading only |
/// | `wake()`'s call to `note_window_event` | delete **or** wrap | **nothing** — reading only. Deleting it is silent under `cargo test` (the state-machine test is itself a caller, so no dead-code lint fires); only a non-test `cargo check` warns, and CI carries no `-D warnings` |
/// | `about_to_wait`'s call to `wake_interval` | delete | the compiler (`now + ..` needs the value) |
/// | `about_to_wait`'s call to `wake_interval` | *substitute* a constant | **nothing** — reading only |
///
/// The three "nothing" rows are the honest residue, and every row above was run in both directions
/// and observed, not assumed. What they have in common is that they are call sites in functions no
/// test in this repo can reach — `render_frame`, `wake` and `about_to_wait` need a GPU and a window.
/// What changed this round is that the *state* they act on left `App`, so the transitions themselves
/// are now driven by a test instead of read; the residue is three one-line call sites rather than
/// the whole mechanism. It is not claimed to be guarded.
#[derive(Default)]
struct SurfaceRetry {
    /// Consecutive `surface.get_current_texture()` failures — reset to 0 by a successful acquisition
    /// and by `note_window_event` (any window event: input, resize, focus).
    consecutive_failures: u32,
}

impl SurfaceRetry {
    /// Fold one acquisition outcome into the streak and hand the outcome straight back.
    ///
    /// Taking the `Result` by value and returning it is the point, not a style choice: it makes the
    /// fold **load-bearing** at the call site. `render_frame` has no other route from
    /// `surface.get_current_texture()` to the `wgpu::SurfaceTexture` it needs, so the mutation that
    /// defeated the previous shape — wrapping the update in `if std::hint::black_box(false) { .. }`,
    /// which left the suite green and `cargo check` silent — no longer compiles here, and neither
    /// does deleting the expression. Stated exactly, because the claim it replaces was overstated:
    /// what this does **not** stop is a refactor that *unwraps* the call and matches on
    /// `surface.get_current_texture()` directly. That compiles and stays green; it is guarded by
    /// reading, like the other two call sites in the table above.
    ///
    /// The old shape was a bare `self.surface_fail_streak = ..` statement, whose deletion produced a
    /// dead-code *warning* and nothing more — and that warning gated nothing: the workflow carries no
    /// `-D warnings`, and one job sets `RUSTFLAGS: ""`.
    ///
    /// Generic in `T` so a test can drive it with `Result<(), _>`; the production instantiation is
    /// `T = wgpu::SurfaceTexture`.
    ///
    /// `saturating_add` is not decoration: a wrapping counter would return to 0 and silently un-arm
    /// the back-off after `u32::MAX` failures, i.e. turn a bound back into a spin.
    fn fold<T>(
        &mut self,
        acquired: Result<T, wgpu::SurfaceError>,
    ) -> Result<T, wgpu::SurfaceError> {
        self.consecutive_failures = App::surface_streak_after(self.consecutive_failures, acquired.is_ok());
        acquired
    }

    /// A window event arrived, so whatever was wrong with the surface may no longer be:
    /// un-minimising, un-occluding and resizing all land here. Clearing the streak restores
    /// full-rate retry immediately, so the back-off never adds latency to a real recovery.
    fn note_window_event(&mut self) { self.consecutive_failures = 0; }

    /// How long `about_to_wait` should sleep, given the streak this holds. See
    /// [`App::next_wake_interval`] for the reasoning; this is the state-carrying wrapper.
    fn wake_interval(&self, active: bool) -> std::time::Duration {
        App::next_wake_interval(active, self.consecutive_failures)
    }
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
    // Minimap. `zone_map` is the map-load OUTCOME carried whole, not a map beside a separately
    // droppable reason (#873, #877 rounds 2–3) — see `zone_map::ZoneMapLoad` for why and for the
    // bound of what the newtype prevents, and `hud_zone_map_view` for how it becomes pixels.
    // `ZoneMapLoad::not_attempted()` until a load has reported.
    zone_min:      [f32; 2],
    zone_max:      [f32; 2],
    zone_map:      zone_map::ZoneMapLoad,
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
    /// Zone-in orchestration: the one-shot reground armed on every zone change, which settles the
    /// player onto the NEAREST floor (above or below) on the first frame with loaded collision —
    /// fixing zone-ins where the zone-point z is below the actual floor (the per-frame snap only
    /// probes downward and can't lift them).
    ///
    /// The logic lives in [`crate::zone_in::ZoneIn`] rather than inline here so it can be run
    /// without a window, a GPU or an event loop: `src/zone_in.rs`'s test module drives a whole
    /// zone-in — arrival, load window, correction, first grounded frame — against real collision
    /// (#712, #728, #745). All `App` still owns is a single unconditional `on_frame` call in
    /// `render_frame` — arming and "the controller has been stepped" are derived inside `ZoneIn`
    /// from arguments, not signalled by extra calls, because a call site that exists is a call site
    /// that can be silently disabled (#791 round 2). That one statement is pinned by
    /// `zone_in::tests::the_app_rs_call_into_this_module_is_an_unconditional_statement`, whose doc
    /// states exactly what such a pin can and cannot establish.
    zone_in:            crate::zone_in::ZoneIn,
    last_frame_time:    std::time::Instant,
    fps_frame_count:    u32,
    fps_timer:          std::time::Instant,
    current_fps:        f32,
    /// Event-driven scheduling: render at full rate until this instant, then drop to an idle poll.
    /// Bumped forward by `wake()` whenever something happens (input, packet, animation in flight).
    /// When `now >= active_until` and nothing is pending, the loop only wakes to poll the network
    /// channel — so a still scene costs ~no CPU. See `about_to_wait`.
    active_until:       std::time::Instant,
    /// The surface-retry back-off's state. See [`SurfaceRetry`] for the whole state machine and for
    /// exactly how much of its wiring is pinned by a test and how much is not.
    surface_retry:      SurfaceRetry,
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
    /// The walker's PUBLISHED nav diagnostics snapshot (#608). While the overlay is toggled on
    /// (`nav_debug`), an `Arc` clone is attached to `scene.nav_debug` each frame and the renderer
    /// draws it verbatim as a depth-tested 3D pass (`eqoxide_renderer::nav_overlay`). The render
    /// thread only READS this — the walker (nav thread) is the sole writer.
    nav_debug_view:   crate::nav::diagnostics::NavDebugView,
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
    /// Everything else an agent reads is projected at HTTP read time from the network thread's
    /// `GameState` (#343); this and `skin_cap_downgrades_shared` below are the render loop's own
    /// publications. Publishing world state from a loop whose whole design goal is to STOP RUNNING
    /// when nothing is happening is how `connected: true` survived a dead connection forever.
    frame_profile_shared: crate::ipc::FrameProfileShared,
    /// The renderer's skin-cap downgrades (eqoxide#797), converted from
    /// `EqRenderer::skin_cap_downgrades` into the ipc-facing `SkinCapDowngradeView` and published
    /// for `/v1/observe/debug` → `skin_cap_downgrades` once per frame, same rhythm as
    /// `frame_profile_shared` just above. The conversion happens HERE (not lower in the crate
    /// graph) because `eqoxide-renderer`'s `SkinCapDowngrade` keeps its `source` path private —
    /// this crate is where the renderer and ipc types are both visible.
    skin_cap_downgrades_shared: crate::ipc::SkinCapDowngradesShared,
    // Precomputed zone collision grid: floor grounding, camera collision, nameplate occlusion.
    // Held as Arc and also published to `shared_collision` so the nav thread can read it.
    collision:    Option<Arc<collision::Collision>>,
    /// Shared slot the nav thread reads to gate /goto movement against walls.
    shared_collision: collision::SharedCollision,
    /// The zone terrain+collision LOAD STATE published for `/v1/observe/debug` (#579). This app
    /// (which owns the zone loader) is its only writer; it goes `Pending` on every zone change —
    /// in the very same block that drops the old collision — and only reaches `Ready` in
    /// `maybe_finish_load`, where the meshes are uploaded and the collision grid exists to hand it.
    zone_assets: crate::nav::zone_assets::ZoneAssetStateShared,
    /// Live handles to the spawned zone-asset loader threads (#595 review F3). Kept ONLY so
    /// `watch_for_lost_load` can tell "the download is slow" (thread still running — leave it
    /// alone, however long it takes) from "the result can never arrive" (every loader has exited
    /// and nothing was published — a panic, or a clobbered handoff slot). Pruned as they finish.
    load_threads: Vec<std::thread::JoinHandle<()>>,
    /// Monotonic zone-load counter handed to each loader — see `PendingLoad::gen`.
    load_gen: u64,
    /// Most recent floor_z result. Used as the anchor for the next frame's floor_z query
    /// so the player's visual height is self-consistent and can't be pulled up to a bridge
    /// or ceiling just because the server placed them there.
    last_grounded_z: f32,
    /// Render position last frame [east, north, z], used to derive facing from motion.
    prev_render_pos: [f32; 3],
    /// Per-entity motion smoothing state, keyed by spawn id. See [`EntityMotion`].
    entity_motion: std::collections::HashMap<u32, EntityMotion>,
    /// Estimated nav-driven speed for the visual player position glide (units/s).
    /// Measured via `eqoxide_core::physics::windowed_speed_sample` (#623 live-validation fix —
    /// see that function's doc for why a naive per-frame re-anchor systematically understates
    /// speed); defaults to RUN_SPEED.
    player_nav_speed: f32,
    /// Real-time anchor for the current `player_nav_speed` sampling window: position and
    /// timestamp of the last time the window elapsed and a new sample was taken. Deliberately
    /// separate from `prev_logical_pos` (below), which still updates every frame for the
    /// unrelated per-frame "did we move at all" latch.
    nav_speed_anchor_pos: [f32; 2],
    /// When `nav_speed_anchor_pos` was last set, for speed estimation.
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
    /// Nav diagnostics overlay toggle (#608): while on, the walker's published
    /// `NavDebugSnapshot` is attached to the scene and drawn as a depth-tested 3D pass
    /// (`eqoxide_renderer::nav_overlay`). Initial state from `--nav-debug`; F11 toggles at runtime.
    nav_debug: bool,
    /// The window system: registry-driven windows, per-character layout
    /// persistence, icon atlases, chat state (#162).
    ui_state: crate::ui::UiState,
    /// Asset-sync progress fraction (0.0–1.0) shown on the loading screen; None when not syncing.
    sync_progress: std::sync::Arc<std::sync::Mutex<Option<f32>>>,
    /// Set to Some(Ok(())) when the common-model sync finishes, Some(Err(LoaderFailure)) on failure
    /// — see `LoaderFailure` for why the failure is typed rather than a bare `String` (#616 review
    /// F2: `poll_sync` must tell a panic apart from the pre-existing "no cached fallback" failure).
    sync_done: std::sync::Arc<std::sync::Mutex<Option<Result<(), LoaderFailure>>>>,
    /// True once character models have been loaded from the cache (guards one-time load).
    models_loaded: bool,
    /// Observable health of the background model-sync worker (#616): `None` while it is alive,
    /// `Some(reason)` once it has stopped for any reason (a panic, a login failure, its channel
    /// closing) — see `run_model_sync_worker`. The worker never restarts once dead, so this is a
    /// terminal, persistent signal rather than a transient status line: it is never cleared once
    /// set. Written only by the model-sync-worker thread via `run_model_sync_worker`.
    ///
    /// SHARED by `Arc` identity with `HttpState::model_sync_dead` (#616 review F1) — constructed
    /// once in `main.rs`, not here, and handed in through `App::new`. Serves on
    /// `GET /v1/observe/debug` as `model_sync_dead`.
    model_sync_dead: Arc<Mutex<Option<String>>>,
    /// Terminal common-asset-loader failure, PERSISTENT unlike `load_status` (#616). `load_status`
    /// is a transient line the loading screen shows only while `loading == true`; `poll_sync` clears
    /// `loading` for a PANIC (mirroring how the zone loader's `maybe_finish_load` unconditionally
    /// clears `loading` and hands the real verdict to the separate, persistent `zone_assets` state)
    /// but deliberately NOT for the pre-existing "sync failed, no cached fallback" ordinary failure
    /// — that one keeps holding the loading screen up with the error on screen rather than silently
    /// proceeding into gameplay with no character models (#616 review F2; see `LoaderFailure` and
    /// `poll_sync`). Either way the reason living only in `load_status` would vanish from view the
    /// instant the loading screen stops drawing, so this field is the persistent verdict for the
    /// common-asset path: `None` until a terminal failure, `Some(reason)` forever after.
    ///
    /// SHARED by `Arc` identity with `HttpState::common_assets_failed` (#616 review F1) —
    /// constructed once in `main.rs`, not here, and handed in through `App::new`. Serves on
    /// `GET /v1/observe/debug` as `common_assets_failed`.
    common_assets_failed: Arc<Mutex<Option<String>>>,
    /// The agent-observable asset-sync activity (#715): `None` when no sync is in progress,
    /// `Some(activity)` naming the set and phase of the one that is. Written ONLY through
    /// `asset_sync::sync_set_observed`, whose RAII guard clears it on every exit path.
    ///
    /// SHARED by `Arc` identity with `HttpState::asset_sync` — constructed once in `main.rs`, not
    /// here, and handed in through `App::new`, for exactly the reason spelled out on
    /// `common_assets_failed` above. Serves on `GET /v1/observe/asset_sync`.
    asset_sync_activity: crate::ipc::AssetSyncShared,
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
        acts:            crate::ui::Actions,
        spells:          std::sync::Arc<crate::spells::SpellDb>,
        shared_collision: collision::SharedCollision,
        zone_assets:      crate::nav::zone_assets::ZoneAssetStateShared,
        // #616 review F1: constructed ONCE in main.rs (mirroring `zone_assets` above) and shared —
        // by identity, not by value — with `HttpState`, which is this app's ONLY writer. Do not
        // construct a fresh `Arc::new(Mutex::new(None))` for either of these inside this function;
        // that would sever the identity `main.rs` set up and `/v1/observe/debug` would read `None`
        // forever no matter what this app publishes into its own (unreachable) copy.
        common_assets_failed: Arc<Mutex<Option<String>>>,
        model_sync_dead:      Arc<Mutex<Option<String>>>,
        // #715: same shared-`Arc`-identity rule as the two above — constructed once in `main.rs`
        // and handed to BOTH this app (the sole writer) and `HttpState` (the reader).
        asset_sync_activity:  crate::ipc::AssetSyncShared,
        frame_profile_shared: crate::ipc::FrameProfileShared,
        skin_cap_downgrades_shared: crate::ipc::SkinCapDowngradesShared,
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
        nav_debug_view:   crate::nav::diagnostics::NavDebugView,
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
            zone_map: zone_map::ZoneMapLoad::not_attempted(),
            visual_player_pos: [0.0, 0.0, 0.0],
            prev_logical_pos:  [0.0, 0.0, 0.0],
            last_moved_at:     std::time::Instant::now(),
            camera: CameraState::new([0.0, 0.0, 0.0], 0.0),
            camera_cmd, camera_snapshot, manual_move,
            camera_initialized: false,
            zone_in: crate::zone_in::ZoneIn::default(),
            last_frame_time: std::time::Instant::now(),
            fps_frame_count: 0,
            fps_timer: std::time::Instant::now(),
            current_fps: 0.0,
            active_until: std::time::Instant::now(),
            surface_retry: SurfaceRetry::default(),
            frame_profile: crate::profiling::FrameProfile::default(),
            keys_held: std::collections::HashSet::new(),
            controller: crate::movement::CharacterController::new([0.0, 0.0, 0.0]),
            controller_view, nav_intent, pos_correction, nav_debug_view,
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
            frame_profile_shared, skin_cap_downgrades_shared,
            shutdown, collision: None, shared_collision, zone_assets,
            load_threads: Vec::new(), load_gen: 0,
            last_grounded_z: 0.0,
            prev_render_pos: [0.0, 0.0, 0.0],
            entity_motion: std::collections::HashMap::new(),
            player_nav_speed: 44.0, // default to RUN_SPEED until first measurement
            nav_speed_anchor_pos: [0.0, 0.0],
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
            model_sync_dead,
            common_assets_failed,
            asset_sync_activity,
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
        if self.gpu.is_none() {
            // No renderer yet, so no load will be started — and nothing else will ever move the
            // state off `Pending`. Say so terminally rather than leaving an agent to poll forever.
            *crate::nav::zone_assets::lock_state(&self.zone_assets) =
                crate::nav::zone_assets::ZoneAssetState::failed(&zone_name,
                    "the renderer was not initialised when this zone change arrived, so no asset \
                     load was started. No retry is running.");
            self.loading = false;
            return;
        }

        self.vert_vel  = 0.0;
        self.on_ground = true;

        // testzone is assembled from in-memory debug data — handle it inline.
        if zone_name == "testzone" {
            if let Some((_, renderer)) = &mut self.gpu {
                renderer.upload_zone_assets(&debug_zone::make_debug_zone());
                tracing::info!("renderer: debug zone loaded ({} meshes)", renderer.gpu_meshes.len());
            }
            // NOT `Ready`: the debug zone builds no collision grid at all, so every nav/collision
            // answer here is unavailable — reporting "ready" would be the #579 lie in miniature.
            *crate::nav::zone_assets::lock_state(&self.zone_assets) = crate::nav::zone_assets::ZoneAssetState::failed(
                "testzone",
                "testzone is an in-memory DEBUG zone: its terrain is synthetic and NO collision \
                 grid is built, so nav/collision answers are unavailable (not empty).");
            self.loading = false;
            return;
        }

        // Zone maps (minimap) + water regions come from the asset server's "gamedata" set in the
        // local cache (synced at startup), not from ~/eq_assets.
        let maps_dir    = crate::asset_sync::CacheDirs::resolve().models_dir().join("maps");
        let load_status = self.load_status.clone();
        let pending     = self.pending_load.clone();
        // #579: the loader thread is the LIVE writer of the pending progress line an agent polls.
        let za_state    = self.zone_assets.clone();
        // #715: the loader thread is also the live writer of the agent-observable sync activity.
        let sync_obs    = self.asset_sync_activity.clone();
        // Monotonic load id (#595 review F3). The handoff is a SINGLE slot shared by every loader,
        // so an older loader finishing late could overwrite a newer one's already-published result;
        // the newer zone's reply was then gone for good and the state hung on `Pending`. A loader
        // may only write the slot if it is not displacing a NEWER load.
        self.load_gen += 1;
        let load_gen = self.load_gen;
        let url  = self.asset_server_url.clone();
        let user = self.asset_user.clone();
        let pass = self.asset_pass.clone();

        *load_status.lock().unwrap() = "Connecting to asset server…".to_string();

        // Named for the #380 crash-log panic hook — see `crash` module docs.
        let handle = std::thread::Builder::new().name("zone-asset-loader".into()).spawn(move || {
            // #595 review F3: a panic anywhere below (a corrupt GLB in `from_glb`, an arithmetic
            // trap in `Collision::build`) used to unwind past the ONLY write to `pending_load`,
            // leaving the observable state on `Pending` with a frozen status line FOREVER — the
            // exact "waiting for a `ready` that is never coming" this type exists to prevent. Catch
            // it and hand back a normal failed result so the usual path publishes an honest
            // `Failed` (with the panic message) and clears the loading screen.
            let zone_for_panic = zone_name.clone();
            let pending_for_panic = pending.clone();
            let load_status_for_panic = load_status.clone();
            let za_for_panic = za_state.clone();
            let body = std::panic::AssertUnwindSafe(move || {
            // Mirrors the loading-screen text into the agent-observable `zone_assets` state (#579),
            // but ONLY while that state is still Pending for THIS zone — a loader left over from a
            // previous zone must never overwrite the current zone's state with its own progress.
            let publish_pending = |zone: &str, s: &str| {
                let mut st = za_state.lock().unwrap();
                if matches!(&*st, crate::nav::zone_assets::ZoneAssetState::Pending { zone: z, .. } if z == zone) {
                    *st = crate::nav::zone_assets::ZoneAssetState::pending(zone, s);
                }
            };
            let set_status = |s: &str| {
                *load_status.lock().unwrap() = s.to_string();
                publish_pending(&zone_name, s);
            };

            let cache = crate::asset_sync::CacheDirs::resolve();
            set_status("Connecting to asset server…");
            let loaded = (|| -> anyhow::Result<assets::ZoneAssets> {
                // #731: the login is observed too. It precedes every sync, it can hang, and while it
                // was unobserved `GET /v1/observe/asset_sync` answered "no asset sync is running"
                // for its whole duration — with the HUD showing a loading screen at the same time.
                // One login covers BOTH sets below, which is why it is its own entry rather than a
                // phase of either.
                let sync = crate::asset_sync::login_observed(
                    &url, &user, &pass, &sync_obs, &format!("zone load: {zone_name}"))?;
                set_status("Verifying zone assets…");
                let dl_status = load_status.clone();
                crate::asset_sync::sync_set_observed(&sync, &format!("zone/{zone_name}"), &cache, &sync_obs, &mut |p| {
                    // #708: only `Downloading` carries bytes/elapsed at all — see `SyncProgress`.
                    // A `Verifying` tick has nowhere a rate could be read from, so this branch is
                    // the ONLY place a speed line can be constructed, and every other status write
                    // in this function (`set_status`) is a plain single-line string with none.
                    if let crate::asset_sync::SyncProgress::Downloading { done, total, bytes, elapsed } = p {
                        let mb = bytes as f64 / 1_048_576.0;
                        let mut line = format!("Downloading zone {done}/{total} ({mb:.1} MB)…");
                        if let Some(bps) = crate::asset_sync::download_rate_bytes_per_sec(bytes, elapsed) {
                            line.push('\n');
                            line.push_str(&hud::format_download_rate(bps));
                        }
                        *dl_status.lock().unwrap() = line;
                    }
                })?;
                // Door/object models for clickable doors come from the asset server's
                // "zonedoors/<zone>" set (the raw <zone>_obj.s3d) into the cache — never ~/eq_assets.
                // Best-effort: if it's absent, load_door_models falls back to plain boxes.
                // #715: observed too, even though it feeds no HUD line. It runs INSIDE the zone
                // load, so leaving it unobserved would make the endpoint answer "no sync in
                // progress" during a stretch of the very load an agent is waiting on.
                let _ = crate::asset_sync::sync_set_observed(
                    &sync, &format!("zonedoors/{zone_name}"), &cache, &sync_obs, &mut |_| {});
                set_status("Reading zone geometry…");
                assets::ZoneAssets::from_glb(&cache.models_dir().join(format!("{zone_name}.glb")))
            })();
            let (opt_assets, load_error, zone_min, zone_max) = match loaded {
                Ok(za) => {
                    let (mn, mx) = za.bounds_xy().unwrap_or(([0.0f32;2],[0.0f32;2]));
                    (Some(za), None, mn, mx)
                }
                Err(e) => {
                    tracing::warn!("renderer: zone '{}' load failed: {}", zone_name, e);
                    (None, Some(e.to_string()), [0.0f32;2], [0.0f32;2])
                }
            };

            set_status("Building collision grid…");
            let collision = opt_assets.as_ref()
                .map(|za| Arc::new(build_zone_collision(za, &maps_dir, &zone_name)));

            set_status("Loading minimap…");
            // #816: `ZoneMap::load` (the lossy `Option` wrapper) is gone — `try_load` names WHY a
            // load failed. This is the HUD minimap's OWN copy of the map (distinct from the one
            // `ActionLoop::sync_zone_points` reads for the agent-facing `zone_map_load` disclosure on
            // `/v1/observe/debug`, see that function's doc): a rendering nicety with no HTTP-observed
            // claim riding on it.
            //
            // #873: this used to discard the failure outright with `.ok()`. A present-but-unreadable
            // detail layer (#816 round 2) fails the WHOLE `try_load` even though the base file read
            // fine, so the minimap went from "draws whatever it could" to "renders wordlessly empty"
            // as an unplanned side effect of that loader change. Kept the whole-load refusal itself
            // (drawing map art with no indication some of it is missing would be the same lie one
            // layer further down), but the reason is no longer thrown away: the WHOLE `Result` is
            // handed on and `hud_zone_map_view` decides at render time what the minimap says about
            // it — a short "map data unavailable: …" line for the two present-but-BROKEN causes, and
            // for a zone that simply ships no map, the same quiet blank canvas as always (see that
            // function's doc).
            //
            // #877 round 3: the load is `ZoneMapLoad::attempt` rather than two lines written out
            // here, for the reason `build_zone_collision` above is a named function — while it was
            // inline in this closure, no test could reach it, and the round-2 reviewer proved the
            // consequence by rewriting exactly this line to `.ok().map(Ok)`: the map was kept, the
            // reason silently dropped (#873 verbatim), and the entire workspace stayed green. That
            // rewrite no longer compiles (`ZoneMapLoad` has no constructor taking a `ZoneMap`) and
            // `attempt` itself is pinned by `zone_map_load_attempt_keeps_both_halves_873`.
            //
            // **This LINE is still reached by no test** — it is inside a spawned thread, behind an
            // asset sync and a GPU upload, and the same is true of the `build_zone_collision` call
            // three lines up. What is pinned is what it calls, and what is prevented is keeping the
            // map while dropping the reason; substituting `ZoneMapLoad::not_attempted()` here, or
            // passing the wrong directory, would still pass the suite.
            let zone_map = zone_map::ZoneMapLoad::attempt(&maps_dir, &zone_name);

            set_status("Uploading to GPU…");
            publish_load(&pending, load_gen, PendingLoad {
                gen: load_gen, zone_name, assets: opt_assets, load_error, collision, zone_map,
                zone_min, zone_max,
            });
            });
            if std::panic::catch_unwind(body).is_err() {
                let reason = "the zone-asset loader thread PANICKED while loading this zone \
                              (see the crash log). No retry is running.";
                tracing::error!("APP: zone-asset loader panicked for '{zone_for_panic}'");
                // Route it through the normal handoff so the main thread publishes `Failed`, drops
                // the loading screen and shows the fallback ground — one code path, one verdict.
                *load_status_for_panic.lock().unwrap_or_else(|e| e.into_inner()) = String::new();
                let _ = &za_for_panic; // the verdict is published by `finish_zone_load` on the main thread
                publish_load(&pending_for_panic, load_gen, PendingLoad {
                    gen: load_gen, zone_name: zone_for_panic, assets: None,
                    load_error: Some(reason.to_string()), collision: None,
                    zone_map: zone_map::ZoneMapLoad::not_attempted(),
                    zone_min: [0.0; 2], zone_max: [0.0; 2],
                });
            }
        }).expect("spawn zone-asset-loader thread");
        self.load_threads.push(handle);
    }


    /// Called each frame to check whether the background load thread has finished.
    /// If so, does the GPU upload (must be on the main thread) and clears `loading`.
    fn maybe_finish_load(&mut self) {
        let result = self.pending_load.lock().unwrap().take();
        let Some(load) = result else { return self.watch_for_lost_load() };

        // A reply for a zone we have since LEFT is dropped ENTIRELY — nothing of it may touch the
        // renderer, the minimap, the collision grid or the observable state (#595 review F2). The
        // GPU upload and the `zone_min`/`zone_max`/`zone_map` assignments used to run BEFORE this
        // check, so a slow load landing after a second zone change repainted the terrain and swapped
        // the minimap bounds to the WRONG zone while `zone_assets` read `ready` for the right one —
        // and `/observe/frame` then served a 200 PNG of another zone with the gate's blessing.
        if load.zone_name != self.current_zone {
            tracing::warn!("APP: dropping a finished load for '{}' — the character is in '{}' now; \
                its terrain/minimap/collision are NOT applied", load.zone_name, self.current_zone);
            return;   // `loading` stays true: the CURRENT zone's own load is still in flight.
        }

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
        // #579: publish the collision grid and the observable verdict TOGETHER (they are derived
        // from the same value inside `finish_zone_load`, so `ready` and the world it describes can
        // never disagree). `ZoneAssetState::ready` refuses to build a `Ready` without terrain
        // meshes AND a collision grid with geometry — a failed/empty load comes out as an explicit
        // `Failed`, never an eternal "pending".
        crate::nav::zone_assets::finish_zone_load(
            &self.shared_collision, &self.zone_assets, &load.zone_name,
            load.collision.clone(),
            load.assets.as_ref().map(|za| za.terrain.len()).unwrap_or(0),
            load.load_error.as_deref());
        self.collision = self.shared_collision.read().unwrap().clone();
        tracing::info!("APP: zone_assets → {:?}", crate::nav::zone_assets::lock_state(&self.zone_assets));
        self.zone_map  = load.zone_map;
        self.loading   = false;
        *self.load_status.lock().unwrap() = String::new();
    }

    /// #595 review F3 — the "stuck in `pending` forever" backstop.
    ///
    /// `Failed` exists so an agent is never left waiting for a `ready` that is not coming, but two
    /// paths used to terminate in NO state at all: a loader thread that **panicked** (it writes
    /// `pending_load` only at the very end, so nothing was ever published), and a loader whose
    /// result was **clobbered** in the single-slot handoff by a second loader and then dropped by
    /// the zone check above. Both leave `zone_assets` on `Pending` with a frozen status line.
    ///
    /// The detector is exact rather than a timeout guess: a loader writes `pending_load` *before* it
    /// returns, so if every spawned loader thread has finished and the slot is still empty while we
    /// are `Pending`, that result can never arrive. (A slow-but-alive download keeps its thread
    /// running and is untouched, however long it takes.)
    fn watch_for_lost_load(&mut self) {
        self.load_threads.retain(|h| !h.is_finished());
        let mut st = crate::nav::zone_assets::lock_state(&self.zone_assets);
        let Some(stuck_zone) = lost_load_zone(!self.load_threads.is_empty(), &st) else { return };
        tracing::error!("APP: zone-asset loader for '{stuck_zone}' exited without reporting a result");
        *st = crate::nav::zone_assets::ZoneAssetState::failed(&stuck_zone,
            "the zone-asset loader thread exited WITHOUT reporting a result (it panicked, or its \
             result was overwritten by a later load). No retry is running — this will never become \
             `ready`. Re-enter the zone or restart the client.");
        drop(st);
        self.loading = false;
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
                Err(f) => {
                    // #616 review F2: `Panicked` and `Ordinary` publish the SAME message (both to
                    // the transient loading-screen text and the persistent `common_assets_failed`
                    // observable — new since the review, so `/v1/observe/debug` shows either kind of
                    // failure) but must NOT clear `self.loading` alike. `Ordinary` is the
                    // PRE-EXISTING "sync failed, no cached models" case: the loader's body ran to
                    // completion and reached this verdict itself, and `poll_sync` has always held the
                    // loading screen up FOREVER for it — a real, actionable, on-screen block, not a
                    // silent degrade. An earlier version of this fix cleared `loading` for both cases
                    // alike, which silently let the client proceed into gameplay with
                    // `models_loaded` still false and nothing else gating rendering on that —
                    // reintroducing, one layer up, exactly the "silently degraded" failure mode #616
                    // exists to remove. Only `Panicked` — the NEW case #616 adds, where the body
                    // never finished so there is nothing more useful to hold the loading screen open
                    // on — clears `loading`. See `common_asset_loader_failure_outcome`.
                    let (msg, clear_loading) = common_asset_loader_failure_outcome(f);
                    *self.load_status.lock().unwrap() = msg.clone();
                    *self.common_assets_failed.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
                    if clear_loading {
                        self.loading = false;
                    }
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
            let dead = self.model_sync_dead.clone();
            let sync_obs = self.asset_sync_activity.clone();   // #715
            std::thread::Builder::new().name("model-sync-worker".into()).spawn(move || {
                // #616: catch_unwind so a panic anywhere below leaves an honest `dead` reason
                // instead of just killing the thread with nothing published — see
                // `run_model_sync_worker`.
                run_model_sync_worker(std::panic::AssertUnwindSafe(move || -> String {
                    let wcache = crate::asset_sync::CacheDirs::resolve(); // same XDG path; cheap
                    // #731: observed. This login serves an UNBOUNDED queue of `charmodel/<key>`
                    // sets over the worker's whole life, so there is no set it could be attributed
                    // to — the reason a login is its own kind of entry and not a sync phase.
                    let sync = match crate::asset_sync::login_observed(
                        &url, &user, &pass, &sync_obs, "model-sync worker (charmodel sets)")
                    {
                        Ok(s) => s,
                        Err(e) => {
                            let reason = format!("model-sync worker: login failed: {e}");
                            tracing::warn!("{reason}");
                            return reason;
                        }
                    };
                    while let Ok(key) = model_rx.recv() {
                        let set = format!("charmodel/{key}");
                        // #715: observed like the other two loaders, so "no sync in progress" is
                        // true of the whole process rather than only of the zone loader.
                        match crate::asset_sync::sync_set_observed(&sync, &set, &wcache, &sync_obs, &mut |_| {}) {
                            Ok(()) => tracing::debug!("model-sync worker: synced {set}"),
                            Err(e) => tracing::warn!("model-sync worker: sync {set} failed: {e}"),
                        }
                    }
                    "model-sync worker: request channel closed (renderer/sender dropped)".to_string()
                }), &dead);
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
        let sync_obs = self.asset_sync_activity.clone();   // #715
        std::thread::Builder::new().name("common-asset-loader".into()).spawn(move || {
            // #616: catch_unwind so a panic anywhere in the body publishes an honest terminal
            // `Err` into `done` instead of leaving it `None` forever — see
            // `run_common_asset_loader`. `done_for_body` is what the body itself writes on a
            // normal finish (success or an ordinary sync error); `done` (the outer clone) is what
            // the wrapper writes ONLY if the body never got that far.
            let done_for_body = done.clone();
            run_common_asset_loader(std::panic::AssertUnwindSafe(move || {
                let result = (|| -> anyhow::Result<()> {
                    // #731: observed, like the zone loader's.
                    let sync = crate::asset_sync::login_observed(
                        &url, &user, &pass, &sync_obs, "common asset load")?;
                    *status.lock().unwrap() = "Verifying assets…".to_string();
                    crate::asset_sync::sync_set_observed(&sync, "common", &cache, &sync_obs, &mut |p| {
                        match p {
                            crate::asset_sync::SyncProgress::Verifying => {
                                // Plain single-line string: no speed line is representable here
                                // (#708) — `Verifying` carries no bytes/elapsed to derive one from.
                                *status.lock().unwrap() = "Verifying assets…".to_string();
                                *progress.lock().unwrap() = None;
                            }
                            crate::asset_sync::SyncProgress::Downloading { done, total, bytes, elapsed } => {
                                let mb = bytes as f64 / 1_048_576.0;
                                let mut line = format!("Downloading {done}/{total} ({mb:.1} MB)…");
                                if let Some(bps) = crate::asset_sync::download_rate_bytes_per_sec(bytes, elapsed) {
                                    line.push('\n');
                                    line.push_str(&hud::format_download_rate(bps));
                                }
                                *status.lock().unwrap() = line;
                                let frac = if total > 0 { done as f32 / total as f32 } else { 1.0 };
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
                        .map(|mut d| d.any(|e| e.map(|e| e.path().extension().is_some_and(|x| x == "glb")).unwrap_or(false)))
                        .unwrap_or(false);
                let final_result = match result {
                    Ok(()) => Ok(()),
                    Err(e) if satisfied => {
                        *status.lock().unwrap() =
                            format!("Asset server unavailable ({e}); using cached models.");
                        Ok(())
                    }
                    // #616 review F2: `Ordinary`, not `Panicked` — the body ran to completion and
                    // reached this verdict itself; `poll_sync` must hold the loading screen open on
                    // it exactly as it did before #616, not treat it like the new panic case.
                    Err(e) => Err(LoaderFailure::Ordinary(
                        format!("Asset sync failed and no cached models: {e}"))),
                };
                *done_for_body.lock().unwrap() = Some(final_result);
            }), &done);
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
    /// Consecutive failed surface acquisitions before the loop backs off to `SURFACE_RETRY_BACKOFF`.
    /// Deliberately larger than a resize blip: at `FRAME_INTERVAL` this is ~128 ms of unchanged
    /// full-rate retry, so the ordinary `Outdated`-during-resize path never reaches the backoff.
    const SURFACE_FAIL_BACKOFF_AFTER: u32 = 8;
    /// Retry cadence once acquisition has failed `SURFACE_FAIL_BACKOFF_AFTER` times in a row. Longer
    /// than `IDLE_POLL` on purpose — see `next_wake_interval`.
    const SURFACE_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_millis(500);

    /// Fold one acquisition outcome into the consecutive-failure streak. Pure so it can be tested.
    /// The only caller is [`SurfaceRetry::fold`], which is what owns the field and what a test
    /// drives; `render_frame`'s call site cannot be reached by a test, since it needs a GPU.
    ///
    /// `saturating_add` is not decoration: a wrapping counter would return to 0 and silently
    /// un-arm the back-off after `u32::MAX` failures, i.e. turn a bound back into a spin.
    fn surface_streak_after(prev: u32, acquired_ok: bool) -> u32 {
        if acquired_ok { 0 } else { prev.saturating_add(1) }
    }

    /// How long to wait before the next loop iteration. Pure so it can be tested; the state-carrying
    /// caller is [`SurfaceRetry::wake_interval`], and `about_to_wait` does nothing else with it.
    ///
    /// The back-off arm is the one worth reading. A pending request that only a render can clear
    /// (`frame_req`, and since #895 `camera_cmd`) makes `poll_external` return true forever while the
    /// surface refuses to hand out a texture, because the thing that would clear it is exactly what
    /// cannot run. Without a backoff that is a full `render_frame` plus a `surface.configure` every
    /// `FRAME_INTERVAL`, unbounded. With it, a persistently failing surface costs one acquisition
    /// attempt per `SURFACE_RETRY_BACKOFF` — which, being longer than `IDLE_POLL`, means a loop
    /// pinned "active" by an unservable request wakes LESS often than an idle one, not more.
    ///
    /// **The back-off is scoped to the ACTIVE base, deliberately** (#895 review B2). The idle arm
    /// issues **no surface acquisition to back off**: `about_to_wait` calls `request_redraw()` only
    /// when `active`, `render_frame` runs only from the `RedrawRequested` arm of `window_event`, and
    /// `render_frame` holds the render loop's only `surface.get_current_texture()` call
    /// (`bin/render_model.rs` has its own, in a separate offscreen binary that never runs this loop).
    /// So an idle wake performs a `try_recv` and nothing else, and stretching it to
    /// `SURFACE_RETRY_BACKOFF` would buy no fewer acquisition attempts — it would only throttle the
    /// network drain from 20 Hz to 2 Hz. That state is reachable and sticky: minimise the window
    /// while anything is animating and ~18 failures accumulate inside `ACTIVE_LINGER`, then activity
    /// lapses; idle requests no redraw, so no `Ok` can ever clear the streak, and `wake()` needs a
    /// window event — absent in exactly the minimised case. An earlier revision capped the idle base
    /// too and an incoming `/v1/camera` would then have waited up to 500 ms before the loop looked at
    /// the slot at all, in the same scenario #895 is about.
    ///
    /// What it does not do: it does not make the loop idle (`active_until` is still in the future),
    /// and it does not bound how long the streak lasts. Recovery is event-driven — `wake()` zeroes
    /// the streak, and it runs on every non-redraw window event, including the resize that un-
    /// minimises the window — so the backoff costs nothing on a real recovery. Nothing bounds it if
    /// the surface fails forever with no window events at all; that case now costs 2 Hz.
    ///
    /// **A residue this does not fix, named rather than papered over.** While the loop is *active*
    /// and backed off, the network drain is still throttled to 2 Hz, because the wake interval is the
    /// only knob this function has and it governs the network poll and the surface retry together.
    /// Decoupling them — keep waking at `FRAME_INTERVAL` for `poll_external`, but issue at most one
    /// `request_redraw()` per `SURFACE_RETRY_BACKOFF` — needs a "last attempt" instant and a change
    /// to `about_to_wait`'s redraw decision, which is a larger change than #895 should carry. What
    /// bounds the damage today is that a real recovery is event-driven and clears the streak.
    fn next_wake_interval(active: bool, surface_fail_streak: u32) -> std::time::Duration {
        let base = if active { Self::FRAME_INTERVAL } else { Self::IDLE_POLL };
        if active && surface_fail_streak >= Self::SURFACE_FAIL_BACKOFF_AFTER {
            base.max(Self::SURFACE_RETRY_BACKOFF)
        } else {
            base
        }
    }

    /// Mark the app active (render at full rate for `ACTIVE_LINGER`) and request a redraw now. Called
    /// from input handlers and whenever `poll_external` finds pending work.
    fn wake(&mut self) {
        self.active_until = std::time::Instant::now() + Self::ACTIVE_LINGER;
        // Clear the surface-retry streak: a window event means whatever was wrong with the surface
        // may no longer be. That THIS line is present is the one transition nothing pins — wrapping
        // it in `if std::hint::black_box(false)` leaves the suite green, measured. See `SurfaceRetry`.
        self.surface_retry.note_window_event();
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
            // Estimate nav-driven speed over a real elapsed window, anchored SEPARATELY from
            // `prev_logical_pos` above (#623 live-validation finding — see
            // `eqoxide_core::physics::windowed_speed_sample`'s doc): `game_state_view.player_x/y/z`
            // is mirrored on essentially every render tick, not just discrete nav steps, so
            // re-anchoring the speed calc every frame (the old code) understated a true 44 u/s run
            // as ~14 u/s and never crossed WALK_RUN_THRESHOLD.
            if let Some(speed) = eqoxide_core::physics::windowed_speed_sample(
                [lp[0], lp[1]],
                self.nav_speed_anchor_pos,
                (now - self.last_player_nav_update).as_secs_f32(),
                eqoxide_core::physics::NAV_SPEED_SAMPLE_WINDOW,
            ) {
                self.player_nav_speed = speed;
                self.nav_speed_anchor_pos = [lp[0], lp[1]];
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
            //
            // The chain itself lives in `select_player_action` (pure, unit-tested below) rather
            // than inline here: `App`/`render_frame` require wgpu+winit and cannot be exercised by
            // `cargo test`, so an inline walk/run branch here is MUTATION-UNDETECTABLE — reverting
            // it to a hardcoded `"walking"` (the exact #623 bug) would leave the whole suite green.
            // Extracting the decision into a free function with primitive inputs makes it directly
            // callable (and therefore red-on-revert) from a unit test (#623 PR review).
            let pid = self.game_state_view.player_id;
            let player_dead = self.game_state_view.cur_hp <= 0 && self.game_state_view.max_hp > 0;
            let swinging = self.game_state_view.combat_anims.get(&pid)
                .is_some_and(|(_, t)| t.elapsed() < crate::scene::COMBAT_SWING_WINDOW);
            let combat_code = self.game_state_view.combat_anims.get(&pid)
                .filter(|_| swinging).map(|(code, _)| *code);
            let moving = self.last_moved_at.elapsed().as_millis() < 250;
            self.scene.player_action = select_player_action(
                player_dead,
                combat_code,
                self.controller.in_water,
                moving,
                self.player_nav_speed,
                self.game_state_view.sitting,
            );
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
                // Just seeded by `teleport`, which drops BOTH the hold and the afloat window, and
                // nothing has stepped yet (#724 review B1). #801 reads them back rather than
                // writing two literal `None`s: this arm flips `initialized = true`, so whatever is
                // in these two fields is immediately mirrored into `GameState` and answered over
                // HTTP — and on a re-spawn/zone-in it would otherwise be the DEPARTED zone's stall,
                // complete with an anchor in coordinates that no longer mean anything. Reading the
                // controller cannot drift from what the controller actually holds, and it makes
                // "seed one, forget the other" fail to compile. See `movement::disclosures`.
                v.publish_disclosures(self.controller.disclosures());
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
            // …and say so (#579): `begin_zone_load` drops the shared collision AND publishes
            // `Pending` for the new zone in one call, so the observable state can never sit
            // stale-`Ready` from the previous zone while the client stands in a terrain-less one.
            self.collision = None;
            // #877 round 2 (finding 5): same reasoning, one field over. `zone_map` is written in
            // exactly ONE place (`apply_load`), and three paths clear `self.loading` without ever
            // reaching it — `watch_for_lost_load`, `reload_zone`'s no-GPU early return, and
            // `reload_zone`'s `testzone` branch (which attempts no map load at all). On those the map
            // window would redraw the PREVIOUS zone's map — and, since #873, the previous zone's
            // failure REASON. The stale-line-art version of this is pre-existing and merely ugly; a
            // stale SENTENCE is a well-formed statement about the wrong zone, which is the failure
            // shape this project ranks highest. Dropping the outcome here closes all three at once,
            // because they all run downstream of this transition.
            //
            // **NO TEST REACHES THIS LINE** (#877 round 3, disclosed here rather than only in the
            // PR body). It sits on the window/GPU path, as does the `self.collision = None;` above
            // it, which is not pinned either. The "written in exactly ONE place" and "all three run
            // downstream" statements above were established by READING the three `self.loading =
            // false` sites, not by measuring them — reasoned-not-measured mechanism claims are this
            // repo's dominant defect class, so treat them as a hypothesis to re-check, and a future
            // edit can delete this line without turning anything red.
            self.zone_map = zone_map::ZoneMapLoad::not_attempted();
            crate::nav::zone_assets::begin_zone_load(
                &self.shared_collision, &self.zone_assets,
                &self.current_zone, "Zone change — starting asset load…");
            // The new zone's floor may sit above the zone-point spawn z; settle onto it once
            // collision loads (see `zone_in::ZoneIn::on_frame`, run in the vertical-physics section
            // below). There is deliberately no `arm()` call here any more: `on_frame` is handed
            // `current_zone` and arms itself the frame that name changes — i.e. this very frame,
            // since the line above is what changes it. A call site that does not exist cannot be
            // written-but-never-reached, which is what #791's round-2 review demonstrated about the
            // source-text pin that used to guard this line (`if false { … }` around it left every
            // test green). See `zone_in`'s module doc, and #799 for the residual.
            //
            // Drop the controller's last-good recovery ring with the old collision (#712).
            // Those samples are untagged coordinates from the PREVIOUS zone; if the fall-through
            // guard or the depenetration fallback restores one here it lands the character at an
            // arbitrary point in THIS zone — in #712 a point 133u outside steamfont's geometry,
            // where it wedged permanently and the nav graph reported `start_isolated`.
            //
            // `ZoneIn::on_frame` clears it again on the first frame it runs. That is a backstop,
            // not a duplicate: this call is the TIMELY one (the ring stops being consultable the
            // instant the old collision goes), and #745 measured that deleting it left the whole
            // suite green, because nothing behavioural reached it. `zone_in::tests::
            // a_zone_in_never_grounds_the_body_on_a_previous_zones_recovery_coordinate` is the
            // behavioural pin on the invariant; `movement::tests::
            // the_zone_change_reload_block_still_forgets_the_recovery_ring` is the textual pin on
            // this line. Do not delete this call as "redundant with the backstop" — between here
            // and there sits the whole ~10s asset load.
            self.controller.forget_recovery_history();
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
        // On a ladder (#309). Read once here beside `in_water` because the manual-drive hatch below
        // needs it for the same reason it needs `in_water`: an agent whose route wedged part-way up
        // a ladder has to be able to finish or abandon the climb by hand, and the climb mechanic is
        // a reconstruction (see `eqoxide_nav::climb`), so leaving it with no manual recovery would
        // be trusting an unverified mechanism with no way out.
        let on_climbable = self.collision.as_ref().is_some_and(|c| c.on_climbable(self.scene.player_pos));
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
                    // No climb key is bound, so free WASD never climbs (#309). Deliberate: a driver
                    // that could set this anywhere is a fly cheat, and the ladder mechanic has no
                    // measured native binding to copy yet — see `eqoxide_nav::climb`.
                    want_climb:  false,
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
                //
                // On a LADDER, `up` drives the climb instead (#309) — the same one field, resolved
                // against whichever medium the body is actually in. A ladder wins over water where
                // both apply (the Crushbone moat is exactly that case): holding the swimmer at its
                // float plane is what a character trying to climb OUT of the moat needs least.
                let vspeed = if on_climbable {
                    m.up * crate::nav::climb::CLIMB_SPEED
                } else if in_water {
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
                    want_climb:  on_climbable && m.up != 0.0,
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

            // One-shot reground after a zone change: if the controller arrived somewhere it can
            // only fall out of the world from, lift it onto the floor above once the new zone's
            // collision is loaded. The #712 case is an arrival a little UNDER the surface whose only
            // ground is below the zone's underworld, which the old "no floor at all below" test read
            // as perfectly settled.
            //
            // The decision — the load throttle, the settled-in-this-zone early return, and the lift
            // itself — lives in `zone_in::ZoneIn::on_frame`, where it can be driven through a whole
            // zone-in without a window or a GPU. Underworld comes from the live world state rather
            // than `controller.underworld` because `set_underworld` is only called from the step
            // below, which does not run while collision is None, so on this first post-load frame
            // the controller still holds the PREVIOUS zone's threshold.
            //
            // This is the ONLY statement in this file that reaches `zone_in`, and it is
            // deliberately unconditional: `current_zone` is what arms it (the frame that name
            // changes), and `camera_initialized` tells it whether the step below will actually run,
            // so both facts are arguments rather than separate calls that could be dropped without
            // anything noticing. Pinned by
            // `zone_in::tests::the_app_rs_call_into_this_module_is_an_unconditional_statement`,
            // which refuses any conditional nesting around this call — see that test's doc for what
            // it does and does not prove.
            self.zone_in.on_frame(
                &mut self.controller,
                &self.current_zone,
                self.loading,
                self.camera_initialized,
                self.collision.as_deref(),
                self.game_state_view.world.zone_underworld,
            );

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
                    self.controller.set_levitating(self.game_state_view.player_levitating());
                    self.controller.step(intent, dt, c);
                    // The controller has now been stepped against THIS zone — the necessary
                    // condition for the zone-in one-shot to read `on_ground` at all (#728 N1).
                    // There is no `note_stepped()` call to make here: this arm's two conditions are
                    // exactly the `camera_initialized` and `collision.is_some()` that `on_frame` is
                    // handed above, so it derives the fact itself, one frame later, rather than
                    // trusting a call that could be deleted silently (#791 round 2).
                    //
                    // Necessary, not sufficient: `step`'s depenetration early return can leave
                    // `on_ground` stale even here, which is why `ZoneIn::on_frame`'s settled arm
                    // also corroborates against this zone's geometry.
                } else {
                    // No collision loaded (mid zone-load): the controller is NOT being stepped, so
                    // whatever hold it last computed describes geometry that has since been dropped
                    // and nothing will recompute it until the new zone lands. Publishing it would be
                    // a confident "you are wedged" about a zone we have left, for the whole ~10 s of
                    // the load. Drop it — unknown, not a stale alarm (#724 review B1).
                    self.controller.clear_hold();
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
                    // #724 review B1 / #801: republish BOTH controller disclosures every RENDERED
                    // frame — level signals, never latched. Each is `None` unless the frame just
                    // stepped re-established it, so this write is also the clear: the frame the body
                    // is freed, `None` overwrites the previous `Some` here with no ceremony. On
                    // frames that render but do NOT step, that `None` comes from the explicit
                    // `clear_hold()` above — which drops the afloat window as well as the hold — not
                    // from a recompute (#724 round-3 review, B1/N1).
                    //
                    // ONE destructuring assignment, not two statements, and that is deliberate
                    // (#801): publishing one disclosure and silently forgetting the other leaves a
                    // stale confident value that `stream_position` keeps mirroring and the API keeps
                    // answering. Written this way, dropping a half does not compile. See
                    // `CharacterController::disclosures`.
                    v.publish_disclosures(self.controller.disclosures());
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

        // The queued `/v1/camera` command is deliberately NOT taken here (#895) — the whole camera
        // block now lives BELOW the `surface.get_current_texture()` match. See the take/apply site
        // right after that match for why, and for the two types that keep it there.
        //
        // Nav diagnostics overlay (#608): while toggled on (--nav-debug / F11), attach the
        // walker's PUBLISHED snapshot to the scene — a cheap Arc clone — and the renderer draws
        // it verbatim as a depth-tested pass. No nav state is derived here: this is wiring only.
        self.scene.nav_debug = if self.nav_debug {
            self.nav_debug_view.lock().unwrap().clone()
        } else {
            None
        };

        let dur_update = prof_update.elapsed();

        // ── GPU work: renderer + egui share a command encoder ─────────────────
        // Use direct field access (not method calls on self) while the GPU
        // borrow is live so Rust can verify field-level disjointness.
        let Some((surface, renderer)) = &mut self.gpu else { return };

        // The acquisition is routed THROUGH the retry state machine — `fold` takes the `Result` and
        // returns it — so every arm below has updated the streak by construction, and short-circuiting
        // that update (`if false { .. }`) is a compile error here rather than a silent no-op. What
        // that does NOT stop is someone unwrapping this back to a bare `surface.get_current_texture()`;
        // `SurfaceRetry`'s doc has the per-mutation table. The streak is what stops a request only a
        // render can service (`frame_req`, `camera_cmd`) from pinning the loop at `FRAME_INTERVAL`
        // while acquisition keeps failing.
        let output = match self.surface_retry.fold(surface.get_current_texture()) {
            Ok(t) => t,
            Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                surface.configure(&renderer.device, &renderer.surface_config);
                return;
            }
            Err(wgpu::SurfaceError::Timeout) => return, // compositor throttling; retry next frame
            Err(e) => { tracing::error!("surface error: {e}"); return; }
        };

        // ── Camera: taken and applied only now that a surface texture is in hand (#895) ────────
        //
        // This block used to sit ABOVE the match — so a `/v1/camera` Set was taken off the queue and
        // written into `self.camera`, and then one of the three arms above could `return` before
        // anything was drawn from it. Worse than a one-tick delay: the take itself was the pending-
        // command signal `poll_external` reports, so clearing it let `about_to_wait` stop asking for
        // redraws once `ACTIVE_LINGER` lapsed. A persistently `Outdated` surface (minimised or
        // occluded window) then meant nothing retried, and the Set was gone.
        //
        // The converse cost of keeping the command queued is real and is bounded deliberately: an
        // un-taken command keeps `poll_external` true, so under that same persistently-`Outdated`
        // premise the loop cannot fall back to `IDLE_POLL`. It is not honest to assert persistence
        // to justify the bug and then assume recovery to bound the fix, so `SurfaceRetry` caps the
        // retry at `SURFACE_RETRY_BACKOFF`. `frame_req` already had this shape before #895; the cap
        // covers it too.
        //
        // Two types hold the ordering, so it survives a refactor that a source-text pin would not
        // see:
        //   * `TakenCameraCmd` re-queues the command on `Drop`, so any `return` between the take and
        //     the apply — including one added later, and including a panic — leaves it PENDING.
        //   * `apply_to` demands an `AcquiredFrame`, which can only be minted from the
        //     `wgpu::SurfaceTexture` above. Moving this take/apply back up the function does not
        //     compile IN A NON-TEST BUILD: there is nothing to pass. The qualifier is load-bearing
        //     and was measured, not assumed — this crate's dev-dependency enables
        //     `eqoxide-renderer/test-fixtures`, and cargo unifies features across the lib and its
        //     test targets, so under `cargo test`/`cargo clippy --all-targets` the escape hatch
        //     `AcquiredFrame::for_test()` IS visible to this production code and an above-the-match
        //     apply compiles. `cargo build --release` (what CI ships, `test.yml`) rejects it, so the
        //     bypass cannot reach a binary — but the mandated test gate is blind to it. This is a
        //     property of every `test-fixtures` token in this repo, `DrawnFrame::for_test`
        //     included; it is not specific to `AcquiredFrame`.
        // See `camera_state::TakenCameraCmd` for the invariant and for what it still does not cover.
        //
        // Not a behaviour change, recorded because it looks like one: `camera.tick(dt, ..)` no
        // longer runs on a skipped tick. Its only `dt`-dependent term is the focus lerp, and
        // `self.camera.focus = cpos` runs unconditionally above the match every tick, so at tick
        // time `focus == player_pos` and the lerp is an identity for every `dt`. The azimuth term is
        // a direct assignment, not an ease. Nothing accumulates and nothing jumps.
        let prof_cam = crate::profiling::Stopwatch::start();
        let taken   = crate::camera_state::TakenCameraCmd::take(&self.camera_cmd);
        let drawing = eqoxide_renderer::AcquiredFrame::from_surface_texture(&output);
        if let Some(taken) = taken {
            taken.apply_to(&mut self.camera, &drawing);
        }
        let (desired_eye, cam_target) = self.camera.tick(dt, self.scene.player_pos, self.scene.player_heading);
        // Camera collision (#852): resolve the eye ONCE, here, and use that single value both
        // for the render below and for the published snapshot. Before the #852 fix the pull-in
        // mutated a local `cam_eye` for rendering only — `snapshot()` re-derived its own eye from
        // `radius`/`focus`, which the pull-in never touched, so a pulled-in frame and the
        // observable an agent reads disagreed 88% of the time a pull-in fired. See
        // `camera_state::resolve_camera_eye`'s doc comment.
        //
        // The snapshot ITSELF is published later, not here — see the write site right after
        // `renderer.render_frame(..)` below, and #867. Moving it back up here does not compile IN A
        // NON-TEST BUILD: `CameraState::snapshot` takes an `eqoxide_renderer::DrawnFrame`, and there
        // is no way to produce one before the draw. Same qualifier, same reason, as the
        // `AcquiredFrame` note 60 lines above — under `cargo test`/`cargo clippy --all-targets` this
        // crate's dev-dependency on `eqoxide-renderer/test-fixtures` makes `DrawnFrame::for_test`
        // visible to this production code, so the move compiles under the test gate and is rejected
        // only by `cargo build --release`. See `eqoxide_renderer::AcquiredFrame::for_test`.
        let resolved = crate::camera_state::resolve_camera_eye(self.collision.as_deref(), cam_target, desired_eye);
        let cam_eye = resolved.eye;
        // Fold this block's cost back into the `update` bucket. It used to be measured there; #895
        // moved it below the acquisition, and the profile overlay's phases would otherwise quietly
        // stop summing to `total`. It is NOT folded into `render`, and the `get_current_texture`
        // wait above stays outside both — attributing a vsync block to "update" would be a far more
        // misleading number than the sliver this preserves.
        let dur_update = dur_update + prof_cam.elapsed();

        let view = output.texture.create_view(&wgpu::TextureViewDescriptor::default());
        let mut enc = renderer.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("frame") },
        );

        let prof_render = crate::profiling::Stopwatch::start();
        let drawn = renderer.render_frame(&mut enc, &view, &self.scene, cam_eye, cam_target, dt);
        let dur_render = prof_render.elapsed();

        // #797 — `render_frame` (via `ensure_character_model`) is what actually populates/updates
        // `renderer.skin_cap_downgrades`, so publish it right here, straight off the call that just
        // ran, rather than reusing the top-of-frame publish point `frame_profile_shared` uses (that
        // point runs BEFORE this call and would publish a stale prior-frame snapshot the first time
        // any given model downgrades). `eqoxide_ipc::SkinCapDowngradeView` is the plain, source-free
        // ipc mirror of `eqoxide_renderer::renderer::SkinCapDowngrade` — this is the one place in the
        // crate graph that can see both types and do the conversion (see the field doc on
        // `skin_cap_downgrades_shared` above).
        {
            let mut view = std::collections::BTreeMap::new();
            for (k, d) in renderer.skin_cap_downgrades.iter() {
                view.insert(k.clone(), crate::ipc::SkinCapDowngradeView {
                    joint_count: d.joint_count,
                    key_collision: d.key_collision,
                });
            }
            *self.skin_cap_downgrades_shared.lock().unwrap() = view;
        }
        // Camera snapshot (#867): published ONLY now that `render_frame` has actually run, and the
        // ordering is enforced by the type system rather than by this line's position in the file.
        // Between `resolved` above and here sits the `surface.get_current_texture()` match, three of
        // whose arms (`Lost`/`Outdated`, `Timeout`) `return` before ever reaching `render_frame`;
        // publishing earlier meant `camera_snapshot` could hold an eye computed for a frame that was
        // never drawn, while its docs claimed "the frame this snapshot describes".
        //
        // `snapshot` takes the `DrawnFrame` token `render_frame` returns, so moving this call above
        // the draw — by hand, or by the extract-method refactor that a source-text pin cannot see —
        // fails to compile: there is no token to pass. That is the whole guarantee; see
        // `eqoxide_renderer::DrawnFrame` for what it does NOT cover (submit/present, and the #422
        // off-screen capture path, which mints a token this site never sees).
        //
        // On a skipped tick nothing is published and the previous snapshot stays. That snapshot is
        // then STALE, by an unbounded amount — `about_to_wait` stops requesting redraws 300 ms
        // (`ACTIVE_LINGER`) after the last activity, so a persistently `Outdated` surface (minimised
        // or occluded window) freezes it indefinitely. It is NOT claimed here that the on-screen
        // image is unchanged over that window: `Outdated` usually means the surface was resized or
        // reconfigured, so what the compositor shows is a stretched/blank presentation, and a
        // minimised window shows nothing at all. The staleness is made DETECTABLE instead of argued
        // away: `drawn_frame`/`drawn_age_ms` in the published struct let a reader tell a fresh
        // snapshot from an ancient one. See `CameraSnapshot`'s doc for the reader-facing side.
        if let Ok(mut snap) = self.camera_snapshot.lock() { *snap = self.camera.snapshot(resolved, drawn); }

        // Cache picking data for the next mouse-click query.
        self.pick_view_proj = renderer.last_view_proj;
        self.pick_cam_eye   = renderer.last_cam_pos;
        self.pick_screen_w  = renderer.surface_config.width;
        self.pick_screen_h  = renderer.surface_config.height;

        // Egui pass — use associated function to avoid reborrowing self.
        let load_status_text = self.load_status.lock().unwrap().clone();
        let sync_frac = *self.sync_progress.lock().unwrap();
        // #873/#877: the map to draw and the reason there isn't one come from ONE call, so an edit
        // that keeps the first and drops the second has to be written out deliberately rather than
        // falling out of a bare `.ok()` (see `hud_zone_map_view`, and `zone_map::ZoneMapLoad` for
        // what is prevented by the type and what is only made conspicuous).
        let (zone_map_view, zone_map_reason) = hud_zone_map_view(self.zone_map.outcome());
        let prof_egui = crate::profiling::Stopwatch::start();
        let egui_wants_repaint = Self::egui_pass(
            &mut self.egui_state, &mut self.egui_renderer, &self.egui_ctx, &mut self.ui_state, &self.window,
            &mut enc, &view, renderer, self.loading, self.fade, &self.current_zone, &load_status_text,
            sync_frac,
            &self.scene, self.zone_min, self.zone_max,
            self.current_fps, zone_map_view, zone_map_reason.as_deref(),
            cam_eye, self.collision.as_deref(),
            &self.acts, &self.spells,
            self.show_debug, self.game_state_view.server_corrections,
            &self.frame_profile,
        );
        let dur_egui = prof_egui.elapsed();

        // Submit — associated function avoids reborrowing self.
        let prof_submit = crate::profiling::Stopwatch::start();
        Self::submit_frame(&self.frame_req, enc, output, renderer, &self.scene, self.camera.focus);
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
        zone_map_error: Option<&str>,
        cam_eye:       [f32; 3],
        collision:     Option<&collision::Collision>,
        acts:          &crate::ui::Actions,
        spells:        &crate::spells::SpellDb,
        show_debug:    bool,
        corrections:   u32,
        frame_profile: &crate::profiling::FrameProfile,
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
                // (#608: the old egui `draw_nav_debug` screen-space overlay is GONE. The nav
                // diagnostics overlay is now a depth-tested 3D pass inside the renderer
                // (`eqoxide_renderer::nav_overlay`), fed from `scene.nav_debug` — see render_frame.)
                ui_state.draw_all(ctx, screen_pts, scene, spells, acts, zone_min, zone_max, zone_map, zone_map_error, current_fps);
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

    /// Submit the command buffer; if a /frame capture is pending, copy the texture to a staging
    /// buffer first and encode it as PNG.
    ///
    /// A capture carrying a per-request camera override (#422) instead renders an EXTRA off-screen
    /// pass with that camera and reads THAT back — the on-screen frame (built above with the live
    /// `CameraState`, unaffected by anything below) is always submitted and presented FIRST,
    /// unconditionally, exactly as if no capture were pending at all.
    ///
    /// That ordering is load-bearing, not cosmetic: `renderer.render_frame` writes the camera
    /// matrix into a single reused GPU buffer (`camera_uniform`) via `queue.write_buffer`, and every
    /// draw call in every pass binds that SAME buffer. Two `write_buffer` calls issued before one
    /// `queue.submit()` both land before that submission's draws execute on the GPU — only the
    /// LAST write would be visible, so recording the override's camera into the same submission as
    /// the on-screen frame would silently paint the on-screen frame with the override's angle. By
    /// submitting the primary frame in its own `queue.submit()` before the override pass even
    /// starts recording, that write is strictly ordered after the primary submission on the queue's
    /// timeline, so the primary draws are guaranteed to see only the live camera. The live
    /// `CameraState` itself is never written either way — this is a render-only, one-shot override.
    fn submit_frame(
        frame_req: &FrameReq,
        encoder:   wgpu::CommandEncoder,
        output:    wgpu::SurfaceTexture,
        renderer:  &mut EqRenderer,
        scene:     &SceneState,
        focus:     [f32; 3],
    ) {
        let pending = frame_req.lock().unwrap().take();
        let Some(eqoxide_ipc::FrameCaptureRequest { camera_override, tx }) = pending else {
            renderer.queue.submit(std::iter::once(encoder.finish()));
            output.present();
            return;
        };

        let w         = renderer.surface_config.width;
        let h         = renderer.surface_config.height;
        let row_pitch = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT
            * ((w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT));
        // 1024 keeps window text readable in captures (#162); 512 made the new UI's 12pt labels
        // illegible. Shared by both the default and override paths below.
        const MAX_DIM: Option<u32> = Some(1024);

        let Some(ov) = camera_override else {
            // Unmodified pre-#422 path: read back the already-rendered on-screen frame.
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
                &slice.get_mapped_range(), w, h, row_pitch, renderer.surface_config.format, MAX_DIM,
            );
            let _ = tx.send(png);
            return;
        };

        // Submit + present the on-screen frame FIRST, completely unmodified — see doc comment above.
        renderer.queue.submit(std::iter::once(encoder.finish()));
        output.present();

        // Now, and only now, record + submit a SEPARATE off-screen pass with the override camera.
        let (eye, look) = crate::camera_state::eye_and_look(ov.azimuth, ov.elevation, ov.radius, focus);
        let offscreen = renderer.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame_capture_override"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: renderer.surface_config.format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let offscreen_view = offscreen.create_view(&wgpu::TextureViewDescriptor::default());
        let mut ov_enc = renderer.device.create_command_encoder(
            &wgpu::CommandEncoderDescriptor { label: Some("frame_capture_override") },
        );
        // dt=0.0: this is a SECOND `render_frame` call within the same real frame — passing the
        // real dt again would double-advance every entity's animation clock for this tick. 0.0
        // draws the exact same pose the primary pass just drew, from a different angle.
        // The returned `DrawnFrame` (#867) is deliberately discarded: this draw goes to an
        // off-screen texture for one PNG, so using its token to publish `camera_snapshot` would
        // name a frame nobody saw at an angle the live camera never held.
        let _off_screen_only = renderer.render_frame(&mut ov_enc, &offscreen_view, scene, eye, look, 0.0);

        let staging = renderer.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("frame_staging_override"), size: (row_pitch * h) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        ov_enc.copy_texture_to_buffer(
            offscreen.as_image_copy(),
            wgpu::ImageCopyBuffer {
                buffer: &staging,
                layout: wgpu::ImageDataLayout {
                    offset: 0, bytes_per_row: Some(row_pitch), rows_per_image: None,
                },
            },
            wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        );
        renderer.queue.submit(std::iter::once(ov_enc.finish()));
        renderer.device.poll(wgpu::Maintain::Wait);
        let slice = staging.slice(..);
        slice.map_async(wgpu::MapMode::Read, |_| {});
        renderer.device.poll(wgpu::Maintain::Wait);
        let png = encode_frame_png(
            &slice.get_mapped_range(), w, h, row_pitch, renderer.surface_config.format, MAX_DIM,
        );
        let _ = tx.send(png);
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

        let now    = std::time::Instant::now();
        let active = now < self.active_until;
        if active {
            // Active: schedule another frame at ~60fps.
            if let Some(w) = &self.window { w.request_redraw(); }
        }
        // Idle: no redraw requested. Wake periodically only to poll the network channel; near-zero
        // CPU. Either way the interval is `SurfaceRetry::wake_interval`'s call, which also throttles
        // the ACTIVE path when the surface has been refusing to hand out a texture (see its doc).
        event_loop.set_control_flow(
            ControlFlow::WaitUntil(now + self.surface_retry.wake_interval(active)),
        );
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
                                    tracing::info!("NAV DEBUG: nav diagnostics overlay {}", if self.nav_debug { "ON" } else { "OFF" });
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
///
/// **The `!=` below is an EXACT, case-SENSITIVE comparison, while `zone_assets::usability` — the
/// function that decides whether the loaded grid may be used to answer about the world — compares
/// the same two zone names with `eq_ignore_ascii_case`. That asymmetry is deliberate, not an
/// oversight (#826).**
///
/// The safety rule the pair has to satisfy is one-directional: *the reload trigger must be at least
/// as eager as the bless test.* If it is, then "no reload is pending" implies "the names match by
/// the bless test too", so a blessed grid is always the zone the character is in. Exact comparison
/// is the most eager comparison there is, so it satisfies that rule **for every non-empty
/// `scene_zone`** — and the cost of the extra eagerness is bounded: a case-only difference can only
/// cause a *spurious* reload — `Pending`, then an honest 503 — never a stale-but-blessed grid
/// (#821 review round 2).
///
/// The empty `scene_zone` case is carried by a different mechanism, not by this comparison: the
/// `is_empty()` short-circuit means an empty `scene_zone` against a loaded `current_zone` starts no
/// reload at all. `usability` is what stays honest there — it reads `player_zone`, not `scene_zone`,
/// and refuses an empty one outright with `PlayerZoneUnknown` (#837 review, N1).
///
/// A case-only divergence cannot arise from the current data flow anyway: `scene.zone` and the
/// `player_zone` handed to `usability` are both copies of the single `gs.world.zone_name`, and
/// `current_zone` is itself a copy of `scene.zone` — so the eagerness is a free margin against a
/// scenario one source already excludes (#837 review, attack 5).
///
/// So do NOT "fix" the inconsistency by making this comparison case-insensitive as well. That does
/// not tighten anything; it drops the pair to exact parity and makes the safety argument depend on
/// the unenforced assumption that two zone shortnames differing only in ASCII case are always the
/// same zone. More seriously, the same edit taken one step
/// further — any comparison here that is *more lenient* than `usability`'s — silently breaks the
/// rule: a real zone change would fail to start a reload while `usability` went on blessing the
/// previous zone's collision grid, which is precisely the stale-ready lie `NotUsable::
/// StaleForPreviousZone` exists to report.
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

/// Selects the self-player's rendered action/clip. Priority: dead > combat swing > swim/tread >
/// walk/run > sitting > idle. Combat and movement override sitting (classic EQ stands you up when
/// you attack or move); sitting only replaces the plain idle clip (eqoxide#53). Pure and
/// unit-tested directly (see `mod tests` below) — this is the ONLY thing standing between the
/// walk/run branch and being mutation-undetectable, since the call site in `render_frame` lives on
/// `App`, which needs wgpu+winit and cannot run under `cargo test` (#623 PR review).
///
/// `moving` is the caller's `last_moved_at.elapsed() < 250ms` latch (nav steps fire every ~150ms;
/// latching "moving" for 250ms keeps the walk/run clip continuous between steps instead of
/// flickering to idle). `combat_code` is `Some` only while a swing animation window is active.
fn select_player_action(
    player_dead: bool,
    combat_code: Option<u8>,
    in_water: bool,
    moving: bool,
    nav_speed: f32,
    sitting: bool,
) -> String {
    if player_dead {
        "dead".to_string()
    } else if let Some(code) = combat_code {
        format!("C{:02}", code)
    } else if in_water {
        // In water we always swim, never stand: the forward stroke (P06 "swim") while moving, and
        // treading water in place (L09 "swim_idle") when holding position — so a still character
        // doesn't appear to stand on the surface (#198/#207).
        if moving { "swimming".to_string() } else { "treading".to_string() }
    } else if moving {
        // Walk vs run is chosen purely by comparing measured speed against WALK_RUN_THRESHOLD
        // (native rule: `speed > walkspeed -> run`, strict). Previously this arm always rendered
        // "walking" regardless of speed, so the run clip (`clip_for_action("running")`) was never
        // requested at any speed (#623).
        eqoxide_core::physics::walk_or_run(nav_speed).to_string()
    } else if sitting {
        "sitting".to_string()
    } else {
        "idle".to_string()
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
                for (d, t) in m.display.iter_mut().zip(to.iter()) { *d += t * f; }
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
                    // #651: pick walk vs. run from the WIRE-NATIVE gait — the server's own speed
                    // code from OP_ClientUpdate — instead of `m.speed`, the position-delta estimate.
                    // `m.speed` is derived from RoF2's sparse, irregular NPC position cadence and
                    // systematically under-reports (it never reliably clears WALK_RUN_THRESHOLD), so
                    // ordinary NPCs never selected the run clip; the gait has no such limitation.
                    // `gait_to_speed` inverts this client's own outbound encoder back to world u/s,
                    // so the SAME threshold that gates the self-player (#623) applies here. Gait is
                    // SIGNED: a backing-up NPC (negative gait) maps to a negative speed and walks,
                    // never runs. Fall back to `m.speed` only when the entity has sent NO position
                    // update yet (`gait` is `None`) — exactly the ambiguous window #643 made explicit.
                    let clip_speed = match b.gait {
                        Some(g) => eqoxide_core::physics::gait_to_speed(g),
                        None    => m.speed,
                    };
                    b.action = eqoxide_core::physics::walk_or_run(clip_speed).to_string();
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
        //
        // #753: the `collision` match runs UNCONDITIONALLY — including while `b.floating` — so
        // the `None` arm's cache invalidation can never be skipped by the floating exemption. The
        // original shape nested the whole match inside `if !b.floating`, so while an entity was
        // floating (a levitate toggle, a boat ride — `floating()` is re-derived from the LIVE
        // flymode every frame, #578, not a one-time spawn flag) the `None` arm was unreachable no
        // matter what `collision` did. Confirmed: the only other `[f32::NAN; 3]` in this file is
        // the `motion.entry(..).or_insert_with(..)` initialiser (~2592), which invalidates only
        // on entry *creation*; nothing else invalidates a live entry. And `self.collision` has
        // exactly two production writers (`Some` on load completion, `None` on reload start), so
        // every real collision swap passes through `None`. `b.floating` now only gates whether
        // the snap is *applied* (the boat/#194 behavior — keep the server-sent z), never whether
        // the `None` arm is reachable.
        //
        // NOT measured: whether a floating entity's cache entry can actually survive a live
        // `Some(A) -> None -> Some(B)` zone swap end to end — `motion.retain` (below) drops an
        // absent entity's entry the first frame it's missing from the billboard list, and
        // `begin_zone_in` clears `world.entities` before `self.collision` goes `None`
        // (`crates/eqoxide-core/src/game_state.rs`). This restructure closes the code-shape hazard
        // (a floating exemption able to bypass an invalidation) regardless of whether that live
        // sequence is reachable today.
        match collision {
            Some(col) => {
                if !b.floating {
                    if b.pos != m.floor_at {
                        m.floor_at = b.pos;
                        m.floor_z  = col.floor_z(b.pos[0], b.pos[1], b.pos[2]);
                    }
                    b.pos[2] = m.floor_z;
                }
                // Boats/ships float on the water surface: keep their server-sent z, do NOT snap to
                // the floor. The server skips FixZ for boats too (Mob::FixZ: `if (GetIsBoat())
                // return;`) because they're GravityBehavior::Floating; floor_z would find the
                // seabed/dock a few units down in shallow harbor water and yank the ship underwater
                // (#194). Left write-free (not memoizing `floor_at`/`floor_z` here) to keep this
                // fix's diff minimal and preserve pre-#753 behavior exactly — memoizing while
                // floating is a plausible alternate design (mutation-checked as M7 in the #753 PR
                // review: the suite stays green under it), but changing it is unrelated to this
                // fix's scope.
            }
            // No collision loaded (zone (re)loading): invalidate the cache so the snap is
            // recomputed against the NEW zone geometry once it arrives, not served stale. Runs
            // regardless of `b.floating` — see above.
            None => m.floor_at = [f32::NAN; 3],
        }
    }

    motion.retain(|id, _| live.contains(id));
}

#[cfg(test)]
mod tests {
    use super::{smooth_entity_motion, zone_needs_reload, next_fade, select_player_action, EntityMotion, MOTION_SMOOTH_DIST};
    use super::{hud_zone_map_view, lost_load_zone, publish_load, PendingLoad};
    use crate::zone_map;
    use std::collections::HashMap;

    fn load(gen: u64, zone: &str) -> PendingLoad {
        PendingLoad {
            gen, zone_name: zone.to_string(), assets: None, load_error: Some("x".into()),
            collision: None, zone_map: zone_map::ZoneMapLoad::not_attempted(),
            zone_min: [0.0; 2], zone_max: [0.0; 2],
        }
    }

    /// #873: the pre-fix code discarded a failed `ZoneMap::try_load` with a bare `.ok()`, so the HUD
    /// minimap went from "draws whatever loaded" to "renders wordlessly empty" the moment #816 round
    /// 2 made a present-but-unreadable detail layer fail the WHOLE load — an unplanned side effect,
    /// not a decision. `hud_zone_map_view` is the fix: it must never lose the reason on a
    /// present-but-BROKEN map, and must never invent one on success.
    ///
    /// It must ALSO stay silent on `Missing` (#877 round 2, owner direction): a zone that ships no
    /// `.txt` map is an ordinary, expected state, not a defect, and **at least 27** zones in the
    /// shipped map pack are in exactly that state. "27" is exact only under the measurement that
    /// produced it — of the 497 zones that ship a `water/<zone>.wtr`, exactly 27 have no base
    /// `<zone>.txt` (see `hud_zone_map_view`); zones shipping neither file were never counted, so
    /// 27 is a floor, not a total. Rendering these identically to a broken layer would report a
    /// non-failure as a failure — the agent-honesty invariant pointing the other way, where a false
    /// alarm is as dishonest as a false success.
    ///
    /// This is the LOGIC half of #873's pin. The rendering half — that the reason reaches the
    /// frame's shape list at all — is `eqoxide-ui`'s
    /// `zone_map_error_reaches_the_frames_shape_list_873`, which walks that list for the laid-out
    /// text. Neither half reaches `src/app.rs`'s loader closure; the load itself is pinned one crate
    /// over by `zone_map_load_attempt_keeps_both_halves_873`.
    #[test]
    fn hud_zone_map_view_keeps_a_defect_reason_and_stays_quiet_on_an_ordinary_absence_873() {
        // ORDINARY absence: no map file at all. Quiet — no reason, nothing for the HUD to paint.
        let missing = Err(zone_map::ZoneMapLoadError::Missing);
        let (map, err) = hud_zone_map_view(Some(&missing));
        assert!(map.is_none(), "a failed load must not fabricate a map");
        assert_eq!(err, None,
            "a zone that simply ships no map is an ORDINARY state, not a failure — giving it a \
             reason paints 'map data unavailable: …' over at least 27 perfectly normal zones and \
             trains a reader to ignore the message that matters");

        // No load has reported yet (the panic backstop, or a zone change in flight): also quiet.
        let (map, err) = hud_zone_map_view(None);
        assert!(map.is_none() && err.is_none(),
            "no load outcome at all must say nothing — inventing a reason here would put a sentence \
             on screen about a load that was never attempted");

        // DEFECTS: the map is there and could not be read. These must carry their own reason, and
        // that reason must name what actually happened — the whole point of keeping it.
        for broken in [
            zone_map::ZoneMapLoadError::Unreadable(std::io::ErrorKind::PermissionDenied),
            zone_map::ZoneMapLoadError::LayerUnreadable("_2", std::io::ErrorKind::InvalidData),
        ] {
            let outcome = Err(broken.clone());
            let (map, err) = hud_zone_map_view(Some(&outcome));
            assert!(map.is_none(), "a failed load must not fabricate a map");
            assert_eq!(
                err.as_deref(), Some(broken.to_string().as_str()),
                "a BROKEN map must carry ITS OWN reason, not a generic/empty one and not another \
                 cause's — this is what lets the HUD show WHY instead of a wordless blank"
            );
        }

        let ok = Ok(zone_map::ZoneMap { lines: Vec::new(), labels: Vec::new() });
        let (map, err) = hud_zone_map_view(Some(&ok));
        assert!(map.is_some(), "a successful load must be kept, not thrown away");
        assert!(err.is_none(), "a successful load must not carry a stale/phantom failure reason");
    }

    /// #595 review F3 — the single handoff slot is shared by every loader. A slow OLD loader
    /// finishing after a newer one must NOT overwrite the newer reply: the main thread would then
    /// drop the stale result on its zone check and the zone the character is actually in would
    /// never get a result at all — an eternal `Pending`.
    #[test]
    fn an_older_load_never_clobbers_a_newer_result() {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        publish_load(&slot, 7, load(7, "qeynos"));      // the newer load lands first
        publish_load(&slot, 3, load(3, "freporte"));    // the older one finishes late
        let held = slot.lock().unwrap().as_ref().map(|l| (l.gen, l.zone_name.clone()));
        assert_eq!(held, Some((7, "qeynos".to_string())),
            "the newer zone's result must survive an older loader finishing after it");
    }

    #[test]
    fn a_newer_load_does_replace_an_older_result() {
        let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
        publish_load(&slot, 3, load(3, "freporte"));
        publish_load(&slot, 7, load(7, "qeynos"));
        let held = slot.lock().unwrap().as_ref().map(|l| l.gen);
        assert_eq!(held, Some(7));
    }

    /// #595 review F3 — the "stuck in `pending` forever" backstop. `Failed` exists so an agent is
    /// never left waiting for a `ready` that is not coming; a loader that panicked (or whose reply
    /// was clobbered) published nothing at all, which used to leave `Pending` frozen forever.
    ///
    /// **What this test cannot do** (#838): it can only construct variants that exist today, so it
    /// says nothing about a `ZoneAssetState` variant added later — once the compiler has forced an
    /// arm for that variant in `lost_load_zone`, this test goes green again whatever the new arm
    /// answers. That gap is covered by the compiler, not here: the arms are written out, so a new
    /// variant reds `src/app.rs` with `E0004` instead of being classified silently. Do not add a
    /// case here that claims to cover it.
    #[test]
    fn a_pending_load_with_no_live_loader_is_declared_lost() {
        use crate::nav::zone_assets::ZoneAssetState;
        let pending = ZoneAssetState::pending("freportw", "Reading zone geometry…");
        assert_eq!(lost_load_zone(false, &pending).as_deref(), Some("freportw"),
            "no loader can ever report this — it must become terminal, not hang");
        assert_eq!(lost_load_zone(true, &pending), None,
            "a slow-but-alive download must be left alone however long it takes");
        // Terminal states are never re-declared lost.
        assert_eq!(lost_load_zone(false, &ZoneAssetState::Idle), None);
        assert_eq!(lost_load_zone(false, &ZoneAssetState::failed("f", "boom")), None);
        assert_eq!(lost_load_zone(false, &ZoneAssetState::test_ready()), None);
    }


    fn bb(id: u32, pos: [f32; 3]) -> crate::scene::Billboard {
        bb_gait(id, pos, None)
    }

    /// Like [`bb`] but with an explicit wire gait, for the #651 walk/run-selection tests.
    fn bb_gait(id: u32, pos: [f32; 3], gait: Option<i32>) -> crate::scene::Billboard {
        crate::scene::Billboard {
            id, pos,
            level: 1, hp_pct: 100.0, is_target: false, dead: false,
            name: format!("npc{id}"), race: "HUM".into(), action: "idle".into(),
            heading: 0.0, equipment: [0; 9], equipment_tint: [[0; 3]; 9],
            gender: 0, face: 0, hairstyle: 0, haircolor: 0, helm: 0, showhelm: 0, floating: false,
            gait,
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
        const { assert!(MOTION_SMOOTH_DIST >= crate::pass::ENTITY_DRAW_DIST,
            "motion gate must be >= draw cull"); }
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

    /// #623 — a remote entity moving faster than `WALK_RUN_THRESHOLD` must render the run clip,
    /// not walk. Exercises the REAL `smooth_entity_motion` path (the same function production code
    /// calls), asserting the actual `b.action` string the renderer will look up via
    /// `Skin::clip_for_action`, not a proxy over the speed math.
    ///
    /// Before the #623 fix this test FAILS: `b.action` was hardcoded to `"walking".to_string()`
    /// whenever `moving` was true, at every speed — verified by reverting the fix locally and
    /// re-running (see the PR body for the exact mutation and its result).
    #[test]
    fn fast_moving_entity_renders_running_not_walking() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let t0 = std::time::Instant::now();
        // Frame 1: seed state at the server pos.
        let mut bbs = vec![bb(7, [0.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t0, 1.0 / 60.0);
        // Frame 2 (~1s later): server pos moved 30u east -> implied speed 30u/s, comfortably
        // above WALK_RUN_THRESHOLD (20u/s) and below RUN_SPEED (44u/s) — a clear "running" case.
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb(7, [30.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert_eq!(bbs[0].action, "running",
            "entity moving at ~30u/s (> WALK_RUN_THRESHOLD) must render the run clip");
    }

    /// #651 — THE fix: walk/run must key on the wire-native `gait`, not the position-delta
    /// `m.speed`. The fixture is deliberately a DISAGREEMENT: the entity's on-screen glide is slow
    /// (~10 u/s, a clear "walking" by `m.speed`, and representative of the sub-threshold speeds the
    /// position estimator produces for ordinary NPCs) while the server-sent gait is a full run
    /// (28 → 44 u/s). Only a code path that reads `gait` selects "running" here; a path reading
    /// `m.speed` selects "walking". Asserts the real `b.action` string the renderer resolves.
    ///
    /// MUTATION CHECK (performed on this branch): reverting the selection at the call site to
    /// `walk_or_run(m.speed)` turns this RED ("walking"), while the None-fallback control below
    /// stays GREEN — proving the assertion discriminates the gait read, not the plumbing.
    #[test]
    fn gait_overrides_slow_position_delta_for_run_selection() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let t0 = std::time::Instant::now();
        // Seed at origin (no gait yet needed — first sight only sets the baseline).
        let mut bbs = vec![bb_gait(7, [0.0, 0.0, 0.0], Some(28))];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t0, 1.0 / 60.0);
        // ~1s later the server pos moved only 10u east → implied m.speed ~10 u/s (BELOW the 20 u/s
        // threshold → walk), but the wire gait says full run (28). Gait must win → "running".
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb_gait(7, [10.0, 0.0, 0.0], Some(28))];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert_eq!(bbs[0].action, "running",
            "gait 28 (full run) must select the run clip even though m.speed ~10 u/s says walk");
    }

    /// #651 control — the None-fallback (an entity that has sent no position update yet, so `gait`
    /// is `None`) must still use the `m.speed` estimate. Same slow ~10 u/s glide as the test above
    /// but `gait: None` → "walking". This is the arm that stays GREEN under the mutation, and it
    /// pins that we did NOT regress the ambiguous "not reported yet" window #643 made explicit.
    #[test]
    fn none_gait_falls_back_to_position_delta() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let t0 = std::time::Instant::now();
        let mut bbs = vec![bb_gait(7, [0.0, 0.0, 0.0], None)];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t0, 1.0 / 60.0);
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb_gait(7, [10.0, 0.0, 0.0], None)];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert_eq!(bbs[0].action, "walking",
            "with no gait reported, walk/run must fall back to the m.speed estimate (~10 u/s → walk)");
    }

    /// #651 sign handling — a backing-up NPC carries a NEGATIVE gait and must NEVER select run,
    /// even when its on-screen glide is fast enough that `m.speed` alone would say "running". The
    /// fixture disagrees on BOTH axes: fast forward glide (~30 u/s), negative (backward) gait.
    /// A path that (wrongly) read the gait as unsigned, or fell back to `m.speed`, would run.
    #[test]
    fn backing_up_negative_gait_never_runs() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let t0 = std::time::Instant::now();
        let mut bbs = vec![bb_gait(7, [0.0, 0.0, 0.0], Some(-28))];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t0, 1.0 / 60.0);
        // 30u glide → m.speed ~30 u/s (would "run" by position delta), but gait is a full-speed
        // BACKWARD run (-28). Signed gait → negative speed → "walking".
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb_gait(7, [30.0, 0.0, 0.0], Some(-28))];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, t1, 1.0 / 60.0);
        assert_eq!(bbs[0].action, "walking",
            "negative gait (backing up) must walk, never run — even with a fast position delta");
    }

    /// #623 companion — the pre-existing submerged override must still win regardless of speed:
    /// a fast-moving entity IN water renders "swimming", never "running". Guards the priority
    /// order (submerged check happens before the walk/run threshold at the call site) against a
    /// future edit that reorders the two.
    #[test]
    fn fast_moving_submerged_entity_still_swims_not_runs() {
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let mut collision = flat_collision_at(-10.0);
        // A real water region spanning z in [-10, 10], well above the floor, so [0,0,0] is wet.
        collision.set_water(Some(std::sync::Arc::new(
            eqoxide_core::region_map::RegionMap::water_slab(-10.0, 10.0))));
        let t0 = std::time::Instant::now();
        let mut bbs = vec![bb(7, [0.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&collision), t0, 1.0 / 60.0);
        // Same 30u/1s = 30u/s fast-move case as `fast_moving_entity_renders_running_not_walking`,
        // now inside the water region — must still be "swimming", never "running".
        let t1 = t0 + std::time::Duration::from_secs(1);
        let mut bbs = vec![bb(7, [30.0, 0.0, 0.0])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&collision), t1, 1.0 / 60.0);
        assert_eq!(bbs[0].action, "swimming",
            "submerged entity must swim regardless of speed, never fall through to running");
    }

    // --- select_player_action (#623 PR review: this is the self-player half of the fix. Nothing
    // in `App::render_frame` is unit-testable — it needs wgpu+winit — so before `select_player_action`
    // was extracted, reverting its walk/run branch to a hardcoded "walking" (the exact reported bug)
    // was mutation-UNDETECTABLE: the whole suite stayed green. These tests call the extracted
    // function directly, so that exact revert is now caught red. Verified: reverting the `moving`
    // arm in `select_player_action` to `"walking".to_string()` (unconditionally) fails
    // `self_player_runs_above_threshold` and `self_player_sitting_only_applies_when_not_moving`
    // (`self_player_walks_below_threshold` asserts `"walking"` and stays green under this specific
    // mutation — it is the other two tests that catch it) — see the PR body for the mutation-check
    // transcript.) ---

    #[test]
    fn self_player_walks_below_threshold() {
        let action = select_player_action(false, None, false, true, 5.0, false);
        assert_eq!(action, "walking");
    }

    #[test]
    fn self_player_runs_above_threshold() {
        let action = select_player_action(false, None, false, true, 44.0, false);
        assert_eq!(action, "running",
            "moving at RUN_SPEED (44 u/s, well above WALK_RUN_THRESHOLD) must select the run clip \
             — this is the exact #623 bug: this arm used to always return \"walking\"");
    }

    #[test]
    fn self_player_dead_overrides_everything() {
        // Dead outranks even a fast-moving, in-combat, submerged, sitting state — all set to what
        // would otherwise select a different action, to prove "dead" really is checked first.
        let action = select_player_action(true, Some(3), true, true, 44.0, true);
        assert_eq!(action, "dead");
    }

    #[test]
    fn self_player_combat_swing_outranks_movement() {
        let action = select_player_action(false, Some(7), false, true, 44.0, false);
        assert_eq!(action, "C07");
    }

    #[test]
    fn self_player_submerged_swims_regardless_of_speed_moving() {
        let action = select_player_action(false, None, true, true, 44.0, false);
        assert_eq!(action, "swimming",
            "submerged + moving must swim, never fall through to the walk/run branch");
    }

    #[test]
    fn self_player_submerged_treads_when_still() {
        let action = select_player_action(false, None, true, false, 0.0, false);
        assert_eq!(action, "treading");
    }

    #[test]
    fn self_player_sitting_only_applies_when_not_moving() {
        let sitting_still = select_player_action(false, None, false, false, 0.0, true);
        assert_eq!(sitting_still, "sitting");
        // Movement stands the player up (classic EQ behavior, eqoxide#53) even while `sitting` is
        // still latched true server-side.
        let sitting_but_moving = select_player_action(false, None, false, true, 44.0, true);
        assert_eq!(sitting_but_moving, "running");
    }

    #[test]
    fn self_player_idle_when_still_and_not_sitting() {
        let action = select_player_action(false, None, false, false, 0.0, false);
        assert_eq!(action, "idle");
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
        const { assert!(1000.0 - 10.0 > MOTION_SMOOTH_DIST, "precondition: entity is out of range"); }
        for _ in 0..2 {
            let mut bbs = vec![bb(8, [10.0, 0.0, 5.0])];
            smooth_entity_motion(&mut motion, &mut bbs, player, Some(&col), now, 1.0 / 60.0);
            assert!(bbs[0].pos[2].abs() < 1e-3,
                "distant entity's billboard must snap to the floor, got z={}", bbs[0].pos[2]);
        }
    }

    /// #194: a FLOATING entity (the boat/ship races — `is_boat_race` → `Entity::floating()` →
    /// `Billboard::floating`) keeps its server-sent z, while a non-floating entity at the same
    /// spot still snaps. A boat rides the water SURFACE; `floor_z` probes 100u down and returns
    /// the seabed/dock beneath it, so snapping would render the ship below the waterline — the
    /// client showing the driving agent a boat where there is no boat. The server skips this for
    /// boats too (`Mob::FixZ` early-returns for `GetIsBoat`, they are `GravityBehavior::Floating`).
    /// BOTH directions are asserted: a widened exemption that swallowed every entity would fail
    /// the second half, so this cannot pass by exempting too much.
    #[test]
    fn floating_entity_keeps_server_z_and_grounded_one_still_snaps() {
        let col = flat_collision_at(0.0); // "seabed" 4u below the surface the boat sits on
        let now = std::time::Instant::now();
        let surface_z = 4.0_f32;

        // Floating (boat): z untouched, and still untouched on the second frame. While floating,
        // the `Some(col)` arm deliberately skips writing `floor_at`/`floor_z` too (#753), so
        // there's nothing for a later frame to resurrect from — the second frame is
        // belt-and-braces, not load-bearing.
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        for _ in 0..2 {
            let mut boat = bb(9, [10.0, 0.0, surface_z]);
            boat.floating = true;
            let mut bbs = vec![boat];
            smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col), now, 1.0 / 60.0);
            assert!((bbs[0].pos[2] - surface_z).abs() < 1e-3,
                "a floating boat must ride its server-sent z={surface_z}, not the seabed at 0.0: got z={}",
                bbs[0].pos[2]);
        }

        // Identical position, NOT floating: still grounded, exactly as before.
        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();
        let mut bbs = vec![bb(9, [10.0, 0.0, surface_z])];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col), now, 1.0 / 60.0);
        assert!(bbs[0].pos[2].abs() < 1e-3,
            "a non-floating entity at the same spot must still snap to the floor, got z={}",
            bbs[0].pos[2]);
    }

    /// #753: a zone-geometry change that happens WHILE an entity is floating must still
    /// invalidate the floor-snap memo, so a later grounded frame — even one that lands back on
    /// the exact bit-identical position — re-raycasts against the CURRENT collision instead of
    /// serving a z computed against geometry that is no longer loaded.
    ///
    /// Sequence, `b.pos` held bit-identical throughout so the ONLY things that change are
    /// `b.floating` and `collision` — exactly the call-site shape the bug needs:
    ///   1. Grounded on `col_a` (floor z=-3): caches the snap.
    ///   2. Entity starts floating (levitate toggle / boat) at the SAME instant a zone reload
    ///      drops `collision` to `None` — the real-world trigger (`self.collision = None` always
    ///      precedes a zone swap, `src/app.rs` zone-reload path).
    ///   3. Still floating, the new zone's collision (`col_b`, floor z=5) arrives.
    ///   4. Entity lands (floating clears) at the SAME position. A correct memo must re-raycast
    ///      against `col_b` and report z=5 — NOT the pre-reload col_a value of z=-3.
    ///
    /// `col_a`'s height is deliberately non-zero (review finding 3, PR #834): `EntityMotion`'s
    /// own zero-init for `floor_z` is also 0.0, so a `col_a` at z=0 couldn't tell "served the
    /// stale col_a raycast" apart from "served the never-initialised default" from the failure
    /// value alone. -3 makes the two cases distinguishable by the number in the panic message.
    #[test]
    fn floating_across_a_zone_reload_does_not_resurrect_the_old_zones_floor() {
        let col_a = flat_collision_at(-3.0);
        let col_b = flat_collision_at(5.0); // different height — any stale serve is detectable
        let now = std::time::Instant::now();
        // z=10 sits ABOVE both floors (-3 and 5) so the downward raycast can find either one;
        // x/y/z is bit-identical across every frame below.
        let p = [10.0, 0.0, 10.0];

        let mut motion: HashMap<u32, EntityMotion> = HashMap::new();

        // 1. Grounded on the OLD zone's collision — caches floor_at=p, floor_z=-3.
        let mut bbs = vec![bb(9, p)];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col_a), now, 1.0 / 60.0);
        assert!((bbs[0].pos[2] + 3.0).abs() < 1e-3, "precondition: grounded on col_a at z=-3");
        // Pin the bit-identity the rest of this test depends on (review finding 4, PR #834): the
        // memo really did cache the raycast at exactly `p`. Steps 2-4 reuse `p` unchanged, so if
        // `m.display`/`m.floor_at` ever drifted by an epsilon, step 4 would re-raycast for the
        // wrong reason and this test would stop pinning #753 while still passing.
        assert_eq!(motion[&9].floor_at, p, "memo must key on the exact position it raycast at");

        // 2. Floating starts exactly as the zone reload drops collision to None (the real
        // trigger path: `self.collision = None` always precedes a zone swap).
        let mut boat = bb(9, p);
        boat.floating = true;
        let mut bbs = vec![boat];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], None, now, 1.0 / 60.0);

        // 3. Still floating, the NEW zone's collision arrives.
        let mut boat = bb(9, p);
        boat.floating = true;
        let mut bbs = vec![boat];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col_b), now, 1.0 / 60.0);

        // 4. Lands at the SAME position. Must re-raycast against col_b (z=5), not resurrect the
        // pre-reload col_a value (z=-3) from a memo that was never invalidated across the change.
        let mut bbs = vec![bb(9, p)];
        smooth_entity_motion(&mut motion, &mut bbs, [0.0; 3], Some(&col_b), now, 1.0 / 60.0);
        assert!((bbs[0].pos[2] - 5.0).abs() < 1e-3,
            "grounded frame after a floating zone-reload transition must re-raycast against the \
             CURRENT collision (col_b, z=5), got z={} — a stale memo would report the pre-reload \
             col_a value of z=-3",
            bbs[0].pos[2]);
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

/// #616: neither background worker had the zone-asset loader's `catch_unwind` protection (added by
/// #595, `src/app.rs`), so a panic in either one just killed the thread with NOTHING published —
/// the observable each worker owns stayed on its last value forever, indistinguishable from "still
/// working". These tests exercise the actual production wrapper functions (`run_common_asset_loader`
/// / `run_model_sync_worker`) with a deliberately panicking body and assert the observable flips to
/// an explicit failure instead. Mutation check: replacing either wrapper's `std::panic::catch_unwind`
/// call with a bare `body()`/`body(...)` call turns both panic tests RED (the panic escapes the test
/// function itself instead of being caught and asserted on).
#[cfg(test)]
mod worker_panic_protection_tests {
    use super::{
        common_asset_loader_failure_outcome, run_common_asset_loader, run_model_sync_worker,
        LoaderFailure,
    };
    use std::sync::{Arc, Mutex};

    /// A panic anywhere in the common-asset-loader body used to unwind straight past the ONLY write
    /// to `done` (the real body's last line), leaving it `None` forever: `poll_sync` never sees a
    /// result, `self.loading` never clears, and the loading screen is frozen on whatever status text
    /// happened to be set right before the panic (e.g. "Verifying assets…", implying progress that
    /// will never come). Must publish `LoaderFailure::Panicked`, specifically — NOT `Ordinary` (#616
    /// review F2): `poll_sync` gives the two variants opposite `self.loading` treatment, so a wrapper
    /// that got the variant wrong would still turn this test green under the earlier (bare-`String`)
    /// version of the check but silently produce the wrong runtime behavior. See
    /// `common_asset_loader_failure_outcome`'s tests below for that half of the contract.
    #[test]
    fn common_asset_loader_panic_publishes_explicit_failure_not_none() {
        let done: Arc<Mutex<Option<Result<(), LoaderFailure>>>> = Arc::new(Mutex::new(None));
        run_common_asset_loader(
            // Simulates a panic partway through the body, BEFORE its own final `*done_for_body.lock()
            // = Some(final_result);` write — the corrupt-manifest/arithmetic-trap shape #616 describes.
            std::panic::AssertUnwindSafe(|| panic!("simulated corrupt manifest mid-sync")),
            &done,
        );
        let got = done.lock().unwrap().clone();
        assert!(
            matches!(got, Some(Err(LoaderFailure::Panicked(_)))),
            "a panic must publish an explicit Err(LoaderFailure::Panicked(_)), not leave `done` at \
             {got:?} — `None` is the original #616 pending-forever hazard (`self.loading` never \
             clears); `Ordinary` would make `poll_sync` treat a real panic like the pre-existing \
             \"no cached models\" case and leave the loading screen stuck forever instead of \
             surfacing the failure"
        );
        let msg = match got { Some(Err(LoaderFailure::Panicked(m))) => m, other => panic!("{other:?}") };
        assert!(msg.to_lowercase().contains("panic"), "failure reason should say it panicked: {msg}");
    }

    /// #616 review F2: `poll_sync` must clear `self.loading` for a panic (nothing more useful for the
    /// loading screen to hold open on) but leave it alone for the pre-existing "sync failed, no
    /// cached models" ordinary failure (the loading screen staying up FOREVER with the error visible
    /// is deliberate, predates #616, and must not be disturbed by this fix — see the doc on
    /// `LoaderFailure`). This test is the direct proof of that distinction: it does not touch a real
    /// `App`, but the pure decision function `poll_sync` calls IS the actual production logic
    /// (`poll_sync` was refactored to delegate to it precisely so this is testable without one).
    #[test]
    fn ordinary_failure_does_not_clear_loading_but_panic_does() {
        let (msg, clear) = common_asset_loader_failure_outcome(
            LoaderFailure::Ordinary("Asset sync failed and no cached models: connection refused".into()),
        );
        assert_eq!(msg, "Asset sync failed and no cached models: connection refused");
        assert!(!clear, "an ordinary sync failure (no cached fallback) must keep holding the loading \
                          screen open — clearing `loading` here would silently let the client proceed \
                          into gameplay with no character models (#616 review F2)");

        let (msg, clear) = common_asset_loader_failure_outcome(
            LoaderFailure::Panicked("the common-asset-loader thread PANICKED".into()),
        );
        assert_eq!(msg, "the common-asset-loader thread PANICKED");
        assert!(clear, "a panic must clear `loading` — the body never finished, so there is nothing \
                         left for the loading screen to usefully hold open on");
    }

    /// A panic in the model-sync-worker used to just kill the thread with NOTHING published — `dead`
    /// stayed `None` forever, identical to "still alive and working" from any caller's perspective, so
    /// on-demand race-model syncing silently stopped forever with no signal anywhere that it died.
    #[test]
    fn model_sync_worker_panic_publishes_explicit_dead_state() {
        let dead: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        run_model_sync_worker(
            std::panic::AssertUnwindSafe(|| -> String { panic!("simulated panic mid-sync") }),
            &dead,
        );
        let got = dead.lock().unwrap().clone();
        assert!(
            got.is_some(),
            "a panic must publish an explicit dead reason, not leave `dead` at None forever — \
             indistinguishable from a healthy, still-running worker"
        );
        assert!(got.unwrap().to_lowercase().contains("panic"));
    }

    /// The non-panic path still works correctly: `run_model_sync_worker` must publish whatever
    /// `body` returns verbatim when it returns normally, not swallow or override a real stop reason
    /// (e.g. "login failed: …") with a generic one.
    #[test]
    fn model_sync_worker_normal_exit_publishes_bodys_reason_verbatim() {
        let dead: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        run_model_sync_worker(
            std::panic::AssertUnwindSafe(|| -> String { "login failed: boom".to_string() }),
            &dead,
        );
        assert_eq!(dead.lock().unwrap().as_deref(), Some("login failed: boom"));
    }
}

/// **#821 review round 2, N2 — the production zone-load wiring, pinned.**
///
/// #803 made "no exits" and "could not read the exits" different TYPES, and the compiler forced
/// every library call site to say which it meant. But the one line in the shipped client that
/// actually feeds region data onto the grid — [`build_zone_collision`]'s `set_region_data` — lived
/// inside a spawned thread closure, so no test could reach it. The reviewer deleted it: the whole
/// root crate stayed green, while a real client would have carried `Err(NotAttached)` on every
/// zone's grid and answered `503` from `/v1/observe/zone_exits` in **every zone in the game**.
///
/// These tests call the production function directly, against a real `.wtr` on disk, and assert the
/// grid can answer the exits question afterwards. They cover the whole wiring, not just the write:
/// the `maps/water/<zone>.wtr` path convention, the loader's `Ok`/`Err`, and the attach.
#[cfg(test)]
mod zone_load_wiring_803 {
    use super::build_zone_collision;
    use crate::assets::{MeshData, RenderMode, ZoneAssets};

    /// A minimal but non-degenerate zone: one floor quad. `Collision::build` needs real triangles
    /// (an empty grid takes a different constructor branch), and `ZoneAssetState::ready` refuses a
    /// grid with none — so this is the shape a production `Ready` zone actually has.
    fn one_floor() -> ZoneAssets {
        let floor = MeshData {
            positions: vec![[-100.0, -20.0, -100.0], [100.0, -20.0, -100.0],
                            [100.0, -20.0, 100.0], [-100.0, -20.0, 100.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }
    }

    /// A v2 `.wtr` byte blob whose whole lower half is one zone-line region carrying `index`.
    ///
    /// Written as BYTES on purpose: the point of this test is the production path from a FILE at
    /// `<maps_dir>/water/<zone>.wtr` to an answerable grid. Handing `build_zone_collision` an
    /// already-parsed `RegionMap` would skip `try_load`, the directory join and the filename
    /// convention — three of the four things that have to be right. Layout is the one documented at
    /// the top of `eqoxide_core::region_map`: magic, u32 version, u32 node count, then per node
    /// `i32 index, 3×f32 normal, f32 split, i32 region, i32 special, i32 left, i32 right,
    /// i32 zone_line_index`.
    fn wtr_with_one_zone_line(index: i32) -> Vec<u8> {
        // (normal, split, special, left, right, zone_line_index). `leaf_at` walks LEFT when
        // `dot(normal, p) + split > 0`, so z > 0 lands on the dry leaf and z < 0 on the zone line.
        // `special == 3` is `REGION_ZONE_LINE` (private to region_map, hence the literal).
        // `(normal, split, special, left, right, zone_line_index)` per BSP node.
        type WtrNode = ([f32; 3], f32, i32, i32, i32, i32);
        let nodes: &[WtrNode] = &[
            ([0.0, 0.0, 1.0], 0.0, 0, 2, 3, 0), // split at z == 0
            ([0.0; 3], 0.0, 0, 0, 0, 0),        // 2: dry leaf (above)
            ([0.0; 3], 0.0, 3, 0, 0, index),    // 3: ZONE LINE leaf (below)
        ];
        let mut out = b"EQEMUWATER".to_vec();
        out.extend_from_slice(&2u32.to_le_bytes());
        out.extend_from_slice(&(nodes.len() as u32).to_le_bytes());
        for (i, (normal, split, special, left, right, zli)) in nodes.iter().enumerate() {
            out.extend_from_slice(&(i as i32).to_le_bytes());
            for c in normal { out.extend_from_slice(&c.to_le_bytes()); }
            out.extend_from_slice(&split.to_le_bytes());
            out.extend_from_slice(&0i32.to_le_bytes()); // region ordinal, unused by the reader
            out.extend_from_slice(&special.to_le_bytes());
            out.extend_from_slice(&left.to_le_bytes());
            out.extend_from_slice(&right.to_le_bytes());
            out.extend_from_slice(&zli.to_le_bytes());
        }
        out
    }

    /// **The healthy path.** A zone whose `.wtr` is where production looks for it must produce a
    /// grid that ENUMERATES its exits. Delete the `set_region_data` call and this reads
    /// `Err(NotAttached)` instead of `Ok([7])`.
    #[test]
    fn a_zone_with_a_wtr_gets_a_grid_that_can_answer_its_exits() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("water")).unwrap();
        std::fs::write(dir.path().join("water/testzone.wtr"), wtr_with_one_zone_line(7)).unwrap();

        let col = build_zone_collision(&one_floor(), dir.path(), "testzone");
        assert_eq!(col.zone_line_indices().as_deref(), Ok(&[7][..]),
            "the loaded region map's zone line must reach the grid — this is what \
             /v1/observe/zone_exits reports and the ONLY production write that puts it there");
        assert_eq!(col.region_data_absent(), None, "the region data IS attached");
    }

    /// **The failure path.** A zone with no `.wtr` must leave the grid able to say WHY — not
    /// `Ok([])` ("this zone has no way out", the #803 falsehood) and not `NotAttached` ("nobody
    /// ever asked", which is a client bug, not a missing asset).
    #[test]
    fn a_zone_with_no_wtr_gets_a_grid_that_refuses_with_the_real_cause() {
        let dir = tempfile::tempdir().unwrap();
        let col = build_zone_collision(&one_floor(), dir.path(), "testzone");

        let absent = col.region_data_absent().expect("a missing .wtr is an absence, not an answer");
        assert_eq!(absent.as_str(), "region_data_missing",
            "the grid must name the LOAD failure. `region_data_not_attached` here would mean the \
             production write was skipped entirely (N2), which reads to an operator as a client \
             bug rather than a missing asset");
        assert!(col.zone_line_indices().is_err(),
            "and the exits question refuses rather than answering the empty list");
    }
}

/// #895 (review B1) — the render loop's back-off: its arithmetic **and** its state machine.
///
/// **Scope, stated up front, and narrowed on purpose.** These tests reach [`App::next_wake_interval`],
/// [`App::surface_streak_after`] and the whole of [`SurfaceRetry`]. They do NOT execute the render
/// loop: no test in this repo can, because `App::render_frame` and `about_to_wait` need a GPU and a
/// window, and `App` itself cannot be constructed here.
///
/// So there are two different things below, and they are pinned to two different strengths:
///
/// * The **decision** — given "is the app active" and "how many acquisitions have failed in a row",
///   what interval comes out — is pinned exhaustively by the sweep.
/// * The **state machine** — that a run of failed acquisitions actually moves the field, that the
///   interval actually changes as a result, and that a success or a window event actually undoes it
///   — is pinned by `the_state_machine_backs_off_after_a_failure_run_and_recovers_895`, which drives
///   `SurfaceRetry`'s real transitions on its real field rather than calling the pure helpers.
///
/// What is **not** pinned, stated because a previous revision of this paragraph claimed otherwise
/// ("enforced by reading and by the compiler") and that claim was false: that `wake()` calls
/// `note_window_event`, and that `about_to_wait` passes `wake_interval`'s result rather than a
/// constant. Those two call sites are guarded by reading only. `render_frame`'s call to
/// [`SurfaceRetry::fold`] *is* compiler-enforced, because `fold` is the only route from the
/// acquisition to the texture. The per-site table is in [`SurfaceRetry`]'s doc.
#[cfg(test)]
mod render_loop_backoff_tests_895 {
    use super::{App, SurfaceRetry};
    use std::time::Duration;

    /// One failed acquisition, in the shape `SurfaceRetry::fold` actually consumes. `T = ()` because
    /// a `wgpu::SurfaceTexture` cannot be built without a GPU — the fold only ever inspects
    /// `is_ok()`, and its genericity exists precisely so this test can supply the outcome without
    /// one.
    fn failed() -> Result<(), wgpu::SurfaceError> { Err(wgpu::SurfaceError::Outdated) }

    /// The relation the whole back-off argument rests on: a loop pinned "active" by a request only a
    /// render can clear must, once the surface is persistently failing, wake **less** often than an
    /// idle loop — otherwise "we bounded it" would be a re-labelling, not a bound.
    #[test]
    fn the_backoff_is_slower_than_idle_which_is_slower_than_a_frame_895() {
        assert!(App::SURFACE_RETRY_BACKOFF > App::IDLE_POLL,
            "a backed-off active loop must cost less than an idle one, else the cap bounds nothing");
        assert!(App::IDLE_POLL > App::FRAME_INTERVAL,
            "idle must be cheaper than active, or the whole schedule is upside down");
        const { assert!(App::SURFACE_FAIL_BACKOFF_AFTER > 0,
            "a threshold of 0 would back off on the first Outdated, i.e. on every ordinary resize"); }
    }

    /// Exhaustive over both activity states and every streak up to well past the threshold.
    ///
    /// Below the threshold the interval must be **exactly** what it was before #895 — that is the
    /// no-regression half, and it is what keeps an ordinary `Outdated`-during-resize blip rendering
    /// at full rate. At or above it, an **active** loop must be capped at `SURFACE_RETRY_BACKOFF` —
    /// that is the bound. Monotonicity is asserted across the whole sweep so no future arm can make
    /// more failures cost more frequent wakes.
    ///
    /// **The idle arm's pin changed in review round 3, and the old one was wrong** (#895 review B2).
    /// It used to assert the cap applied "regardless of activity". That asserted the mechanism where
    /// the mechanism has nothing to act on: the back-off's purpose is to bound *surface acquisition
    /// attempts*, and the idle path issues none — `about_to_wait` calls `request_redraw()` only when
    /// `active`, `render_frame` runs only from `RedrawRequested`, and `render_frame` holds the render
    /// loop's only `surface.get_current_texture()`. So the old idle pin locked in a 10× throttle of
    /// the *network drain* (`poll_external` runs at the wake cadence) that bought zero fewer
    /// acquisition attempts, in a state that cannot self-clear. It is now pinned the other way: an
    /// idle loop wakes at `IDLE_POLL` no matter how long the streak is. Deleting this arm's
    /// `assert_eq!` is what a future "just cap everything" edit would have to do to go green.
    #[test]
    fn a_failing_surface_can_only_slow_the_loop_down_never_speed_it_up_895() {
        let mut checked = 0_usize;
        for active in [true, false] {
            let mut prev = Duration::ZERO;
            for streak in 0..=(App::SURFACE_FAIL_BACKOFF_AFTER + 8) {
                let got = App::next_wake_interval(active, streak);

                if !active {
                    assert_eq!(got, App::IDLE_POLL,
                        "the idle base must never be backed off: an idle wake acquires no surface, \
                         so stretching it throttles only the network drain (streak={streak})");
                } else if streak < App::SURFACE_FAIL_BACKOFF_AFTER {
                    assert_eq!(got, App::FRAME_INTERVAL,
                        "below the threshold the schedule must be untouched (streak={streak})");
                } else {
                    assert!(got >= App::SURFACE_RETRY_BACKOFF,
                        "at/above the threshold an active loop must be capped (streak={streak}): {got:?}");
                    assert!(got >= App::IDLE_POLL,
                        "a pinned-active loop under a dead surface must not outpace an idle one \
                         (streak={streak}): {got:?}");
                }

                assert!(got >= prev,
                    "more consecutive failures must never mean a shorter wait \
                     (active={active}, streak={streak}): {got:?} < {prev:?}");
                prev = got;
                checked += 1;
            }
        }
        // Reach control: without it, a threshold edited to 0 (making the loop range empty) or an
        // arm that became unreachable would report a green "nothing wrong" that is really
        // "nothing looked at".
        assert_eq!(checked, 2 * (App::SURFACE_FAIL_BACKOFF_AFTER as usize + 9),
            "every (activity, streak) pair in the sweep must have been classified");
    }

    /// The streak arithmetic. A success must clear the streak outright — not decrement it — or a
    /// surface that alternates fail/succeed would drift into a permanent back-off; and failures must
    /// saturate rather than wrap, since a wrap to 0 would un-arm the bound.
    #[test]
    fn a_success_clears_the_streak_and_failures_saturate_895() {
        for prev in [0, 1, App::SURFACE_FAIL_BACKOFF_AFTER, 9_999, u32::MAX] {
            assert_eq!(App::surface_streak_after(prev, true), 0,
                "one successful acquisition must clear the streak outright (prev={prev})");
        }
        assert_eq!(App::surface_streak_after(0, false), 1);
        assert_eq!(App::surface_streak_after(App::SURFACE_FAIL_BACKOFF_AFTER, false),
                   App::SURFACE_FAIL_BACKOFF_AFTER + 1);
        assert_eq!(App::surface_streak_after(u32::MAX, false), u32::MAX,
            "the counter must saturate: wrapping to 0 would silently un-arm the back-off");
        assert!(App::next_wake_interval(true, App::surface_streak_after(u32::MAX, false))
                    >= App::SURFACE_RETRY_BACKOFF,
            "and the saturated value must still read as backed-off");
    }

    /// The threshold itself, pinned on both sides. `>=` vs `>` is a one-character edit that no
    /// range-wide assertion above would notice on its own.
    #[test]
    fn the_backoff_starts_exactly_at_the_threshold_895() {
        let below = App::next_wake_interval(true, App::SURFACE_FAIL_BACKOFF_AFTER - 1);
        let at    = App::next_wake_interval(true, App::SURFACE_FAIL_BACKOFF_AFTER);
        assert_eq!(below, App::FRAME_INTERVAL,
            "one failure short of the threshold must still render at full rate");
        assert_eq!(at, App::SURFACE_RETRY_BACKOFF,
            "the threshold-th consecutive failure is what arms the back-off");
    }

    /// **The wiring, driven rather than read** (#895 review B1).
    ///
    /// Every test above calls a pure helper with a streak value handed to it. This one never names a
    /// streak: it starts from a fresh [`SurfaceRetry`], pushes real acquisition outcomes through
    /// [`SurfaceRetry::fold`] — the same method `render_frame` calls, on the same field — and asks
    /// [`SurfaceRetry::wake_interval`] what the loop would do. So it fails if the increment, the
    /// clear-on-success, the clear-on-window-event or the threshold comparison stops happening,
    /// including when the offending statement is *wrapped* rather than deleted. That is the gap
    /// round-2 review measured: with the streak as a bare `App` field, wrapping the increment in
    /// `if std::hint::black_box(false)` left the suite fully green and `cargo check` silent.
    ///
    /// It does **not** pin that `render_frame`, `wake` and `about_to_wait` call these methods. One of
    /// those three is compiler-enforced and two are not; the per-site table is on [`SurfaceRetry`].
    #[test]
    fn the_state_machine_backs_off_after_a_failure_run_and_recovers_895() {
        let mut retry = SurfaceRetry::default();
        assert_eq!(retry.wake_interval(true), App::FRAME_INTERVAL,
            "a fresh loop renders at full rate");

        // Below the threshold: the run is accumulating, and it must change nothing yet.
        let mut folded = 0_usize;
        for _ in 1..App::SURFACE_FAIL_BACKOFF_AFTER {
            assert_eq!(retry.fold(failed()), Err(wgpu::SurfaceError::Outdated),
                "fold must hand the acquisition straight back — that return value is the ONLY route \
                 from `get_current_texture` to the texture in `render_frame`, and is what makes \
                 dropping this call a compile error instead of a dead-code warning");
            folded += 1;
            assert_eq!(retry.wake_interval(true), App::FRAME_INTERVAL,
                "a partial failure run must not slow the loop (after {folded} failures)");
        }
        assert_eq!(folded, App::SURFACE_FAIL_BACKOFF_AFTER as usize - 1,
            "reach control: the sub-threshold run must have actually executed");

        // The threshold-th failure is the one that arms it. This is the observation the whole
        // finding was about: the interval CHANGES because failures were pushed through the field.
        retry.fold(failed()).unwrap_err();
        assert_eq!(retry.wake_interval(true), App::SURFACE_RETRY_BACKOFF,
            "the {}th consecutive failure must back the active loop off",
            App::SURFACE_FAIL_BACKOFF_AFTER);
        assert_eq!(retry.wake_interval(false), App::IDLE_POLL,
            "…and must leave the IDLE cadence alone: an idle wake acquires no surface, so backing \
             it off would throttle only the network drain (#895 review B2)");

        // Recovery path 1 — a window event (un-minimise, resize, focus, any input).
        retry.note_window_event();
        assert_eq!(retry.wake_interval(true), App::FRAME_INTERVAL,
            "a window event must restore full-rate retry immediately, or the back-off adds latency \
             to a real recovery");

        // Recovery path 2 — a successful acquisition, from a saturated streak, so this also pins
        // that success CLEARS rather than decrements.
        for _ in 0..(App::SURFACE_FAIL_BACKOFF_AFTER + 5) { retry.fold(failed()).unwrap_err(); }
        assert_eq!(retry.wake_interval(true), App::SURFACE_RETRY_BACKOFF,
            "re-arm control: the second failure run must have armed it again, or the assertion \
             below would pass on a machine that never backed off");
        assert_eq!(retry.fold::<()>(Ok(())), Ok(()),
            "fold must pass a successful acquisition through unchanged");
        assert_eq!(retry.wake_interval(true), App::FRAME_INTERVAL,
            "one success must clear the whole run outright — a decrement would leave a surface that \
             alternates fail/succeed permanently backed off");
    }
}
