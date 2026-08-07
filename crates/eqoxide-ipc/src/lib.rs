//! `eqoxide-ipc` — the "inter-thread contracts" crate: the request-slot types shared between the
//! HTTP API thread, the network (login/gameplay/navigation) thread, and the render/app loop.
//!
//! Extracted as the second member of the Cargo workspace (#544 Step 2c). It sits directly above
//! `eqoxide-core` and below everything else — the layering is `core ← ipc ← {net, render, http,
//! command, …}` — and depends ONLY on `eqoxide-core` plus the low-level channel/serde primitives
//! (`tokio::sync::oneshot`, `arc-swap`, `serde`) its slot types are literally made of. It never
//! reaches up into wgpu/winit/egui/eq_net/renderer/app. The app crate re-exports this crate as its
//! `ipc` module (`pub use eqoxide_ipc as ipc`), so existing `crate::ipc::…` paths across the tree
//! keep resolving unchanged.
//!
//! These are `Arc<Mutex<Option<T>>>`-style shared cells an HTTP handler writes a request into and
//! the network action loop (or, for a few render-owned values, the app loop) drains each tick, plus
//! the matching "published snapshot" direction (`Arc<Mutex<T>>` / `Arc<ArcSwap<T>>`) the network
//! thread writes and HTTP/render read. They are neither genuine HTTP types (route state, request/
//! response bodies — those stay in the app crate's `http`) nor genuine network-protocol types — this
//! crate is the neutral third party both sides depend on, so the network loop no longer has to reach
//! into `http` for its own inter-thread plumbing.
//!
//! ## Relocated shared type definitions (#544 Step 2c)
//! Several of this crate's slots wrap type *definitions* that used to live in higher app-crate
//! modules, forcing an up-reference out of `ipc`. Those pure-data definitions moved DOWN here (their
//! BEHAVIOR stayed in the app crate, which now `use`s these types — the correct app → ipc direction):
//! - `MoveIntent`, `ControllerView` — from `movement` (the `CharacterController` stepping logic stays).
//! - `CameraMode`, `CameraCmd`, `CameraSnapshot` — from `camera_state` (the `CameraState` update logic stays).
//! - `FrameProfile`, `FrameSample` — from `profiling` (the `Stopwatch` collection helper stays).
//! - `enabled`/`set_enabled` (the profiling on/off toggle) — from `profiling` (#544 Step 2o), so the
//!   new `eqoxide-ui` crate (which reads it once per window to gate a timing log) does not need an
//!   up-reference into the app crate just for a boolean flag. `Stopwatch` stays in `profiling`.
//!
//! Each origin module re-exports its moved types (`pub use eqoxide_ipc::…`) so every existing
//! `crate::movement::MoveIntent` / `crate::camera_state::CameraCmd` / `crate::profiling::FrameProfile`
//! path is unaffected. Serde derives/attrs/field names were preserved verbatim (several are
//! serialized to the HTTP JSON API — the wire form must not change).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

/// The A3 Command-with-result outcome types (#557 — `ipc` owns them because its own await-slot
/// aliases below reference them; see `result`'s module doc for why they moved down out of
/// `command_state`, and `command_state`'s re-export for why every existing call site is unaffected).
pub mod result;
pub use result::{BuyOk, CastEnd, CommandResult, GiveOk, OpenOk};

/// The agent-observable asset-sync activity slot (#715) — written by the app crate's asset-sync
/// wrapper, read by `GET /v1/observe/asset_sync`. Lives here for the same reason as every other
/// slot in this crate: it is a contract between the app/loader threads and the HTTP thread, and
/// `eqoxide-http` must be able to name it without depending on the app crate above it.
pub mod asset_sync;
pub use asset_sync::{
    AssetConnectGuard, AssetSyncActivity, AssetSyncGuard, AssetSyncPhase, AssetSyncShared,
    AssetSyncSnapshot, AssetSyncWork, ConnectOutcome, EndedWhat, LastLoginByOutcome,
    LoginOutcomeTally, RetainedLogin,
};

// ── Relocated shared type definitions (#544 Step 2c) ─────────────────────────────────────────────
// Pure-data types the slots above/below wrap, moved down out of the app crate so `ipc` no longer
// up-references `movement`/`camera_state`/`profiling`. Definitions only — the behavior that operates
// on them (controller stepping, camera update, frame-profile collection) stays in those app modules,
// which re-export these. Derives/serde attrs/field visibility are byte-identical to the originals.

/// What the driver wants this frame. `wish_dir` is a horizontal direction in server axes
/// (east, north); magnitude is treated as a throttle (clamped to 1). `speed` is run speed (u/s).
///
/// Relocated from `movement` (#544 Step 2c); `movement::CharacterController::step` consumes it.
#[derive(Clone, Copy, Debug, Default)]
pub struct MoveIntent {
    pub wish_dir:    [f32; 2],
    pub wish_vspeed: f32,
    pub jump:        bool,
    pub want_swim:   bool,
    pub speed:       f32,
    /// Max step-up height the controller may climb this move, in EQ units. `0` (default) uses the
    /// native `movement::STEP_UP` (2.0) — correct for free WASD, which must NOT be able to scale
    /// walls. The `/goto` planner raises it so the controller can surmount the small lips
    /// (fences/cart edges) that `find_path` already routed over (its edge-climb cap is the same).
    /// Without this the path leads over a lip the 2u step can't clear and the player wedges (#41).
    pub climb:       f32,
    /// One-shot request to hop a low barrier (fence/cart) this tick. The `/goto` planner sets it once
    /// its own net-progress stall detection fires (the controller can't see net progress — sliding
    /// ALONG a fence looks like good per-frame motion). The controller hops only if it's grounded,
    /// off cooldown, and a near-level landing exists just beyond (`movement::CharacterController::can_hop`).
    /// Free WASD leaves it `false` (a player walking into a wall shouldn't auto-jump). Fixes the Halas
    /// sled-pen (#41).
    pub hop:         bool,
}

/// A read-only snapshot of the controller the render thread publishes each frame for the nav
/// thread to stream to the server (design §2 "Threading"). `heading` is EQ-CCW degrees.
///
/// Relocated from `movement` (#544 Step 2c); the render thread produces it, the nav thread reads it.
#[derive(Clone, Copy, Debug, Default)]
pub struct ControllerView {
    pub pos:     [f32; 3],
    pub heading: f32,
    /// False until the render thread has spawned and seeded the controller. The nav streamer must
    /// not mirror/stream a default (origin) position before this is set.
    pub initialized: bool,
    /// One-shot fall height (feet dropped) latched by the render thread the frame the controller
    /// LANDS from an airborne stretch, for the nav thread to apply driver-agnostic fall damage (§442,
    /// #442). `None` except right after a landing; the nav streamer take-and-clears it exactly once.
    /// Respects the init gate — default `None`, only ever set after `initialized`.
    pub landed_fall_height: Option<f32>,
    /// #724 review B1: the controller is holding the body still and has no way to resume — see
    /// [`eqoxide_core::game_state::ControllerHold`]. Republished from
    /// `CharacterController::hold()` on every RENDERED frame (not latched like
    /// `landed_fall_height`, which is a one-shot the reader must not miss): this is a level signal,
    /// so a stale `Some` after the condition ends is the failure mode to avoid, and re-publishing
    /// the current value is what avoids it. `ActionLoop::stream_position` mirrors it into
    /// `GameState::player_hold`.
    ///
    /// "Rendered" is load-bearing (#724 round-3 review, N1 — this said "EVERY frame" flatly).
    /// `about_to_wait` has an explicit idle branch that renders nothing, and on rendered frames
    /// where the controller is not stepped (no collision, mid zone-load) the level signal is
    /// supplied by an explicit `clear_hold`, not by a recompute. Both are pinned by name; see
    /// `CharacterController::clear_hold`.
    hold: Option<eqoxide_core::game_state::ControllerHold>,
    /// #776/#801: the controller is afloat, being wished at, and going nowhere — see
    /// [`eqoxide_core::afloat::AfloatStall`]. **Not a weaker `hold`, and not a stronger one
    /// either: a different claim.** `hold` says the body cannot move at all under any driver;
    /// this says only that *this wish* has produced no motion for this long, which is why they are
    /// two fields and not one enum. A swimmer at the qcat pocket mouth stalls a horizontal wish
    /// forever and still escapes under a driven dive.
    ///
    /// Published on exactly the same terms as `ControllerView::hold`, by the same statement:
    /// `app.rs` destructures `CharacterController::disclosures()` into both fields at once, so this
    /// is a level signal republished every RENDERED frame, and the republish is also the clear. On
    /// rendered frames that do not step the controller (no collision, mid zone-load) the value comes
    /// from `CharacterController::clear_hold`, which drops the stall window as well as the hold —
    /// not from a recompute. `ActionLoop::stream_position` mirrors it into
    /// `GameState::player_afloat_stall`, which `GameState::begin_zone_in` clears so a departed
    /// zone's claim cannot survive a zone load during which the render loop published nothing.
    ///
    /// Deliberately NOT the shape the now-deleted `moving` field had (#746): that field recomputed
    /// `!on_ground` unconditionally on every publish, so on a rendered-but-not-stepped frame (no
    /// collision, mid zone-load) it republished the PREVIOUS zone's answer as if it were current —
    /// the reason it was deleted rather than wired to a reader. This field does not have that gap:
    /// it clears through `CharacterController::clear_hold` on the same not-stepped path.
    afloat_stall: Option<eqoxide_core::afloat::AfloatStall>,
}

impl ControllerView {
    /// Read both controller disclosures. The only way to see either from outside this crate.
    ///
    /// Returns them in the same order [`eqoxide_core::game_state::ControllerHold`] then
    /// [`eqoxide_core::afloat::AfloatStall`] that `movement::CharacterController::disclosures`
    /// produces, so the mirror in `ActionLoop::stream_position` is one destructuring assignment.
    pub fn disclosures(
        &self,
    ) -> (Option<eqoxide_core::game_state::ControllerHold>, Option<eqoxide_core::afloat::AfloatStall>)
    {
        (self.hold, self.afloat_stall)
    }

    /// Publish both controller disclosures. The only way to write either from outside this crate.
    ///
    /// # Why these two fields are private and every sibling is `pub` (#801)
    ///
    /// Because forgetting one of them is *silent*. Both are level signals: the render thread
    /// republishes them every rendered frame, and the republish IS the clear. A publisher that
    /// updates one and not the other leaves the other holding its previous value, which
    /// `ActionLoop::stream_position` keeps mirroring and `GET /v1/observe/debug` keeps confidently
    /// answering — the #343 `connected: true` shape, where a well-formed field lies in exactly the
    /// window that matters. Nothing recomputes it and nothing looks wrong.
    ///
    /// This was MEASURED on #801 rather than assumed. With the fields public, replacing `app.rs`'s
    /// paired write with a `v.hold = self.controller.hold();` that never touches `afloat_stall`
    /// compiled and left the entire workspace green — 54 targets, 1772 passed, 0 failed. **That
    /// figure is anchored to commit `efed5e2`, the change that made these fields private, and it
    /// cannot be re-run at any later head**: the mutation requires the fields to be public, which is
    /// precisely what that commit removed. Do not read it as a claim about this head's suite. (#810
    /// round-2 review, N3 — a number in a tracked file with no provenance reads as authority.)
    /// `app.rs`'s
    /// frame loop needs a GPU and a window, so no unit test can reach that statement, and this
    /// repo's `include_str!` source pins have six separately measured evasions. Privacy is the one
    /// mechanism here that a diff cannot slip past: that mutation is now
    /// `error[E0616]: field 'hold' of struct 'ControllerView' is private`.
    ///
    /// **What this does NOT do**, stated so nobody reads more into it: it does not prove the
    /// publisher runs, and it does not stop a caller passing a deliberate `None`. It removes
    /// exactly one failure — updating one disclosure while silently leaving the other stale — and
    /// makes writing only one a compile error rather than an omission a reviewer has to notice.
    ///
    /// **Say the residual out loud rather than let the paragraph above imply it away (#801 round-2
    /// review, N1): deleting the whole `v.publish_disclosures(self.controller.disclosures());` call
    /// in `app.rs`'s stepped arm leaves the workspace fully green** — **re-measured at this head**
    /// (`ae49d2b`), not carried forward: 54 target headers, 54 `test result:` lines, **1772 passed,
    /// 0 failed, 47 ignored, 0 filtered out**, identical in every figure to the unmutated run. This
    /// paragraph previously quoted 1774, which was round 1's total and was stale by two tests; the
    /// mutation was re-run rather than the number patched, because patching it would have been the
    /// reasoned-not-measured move (#810 round-2 review, N3). Severing only the mirror one layer down IS caught (`stream_position`'s test goes
    /// red), and severing one field of two is now a compile error, but *removing the call site* is
    /// caught by nothing in CI. `app.rs`'s frame loop needs a GPU, a window and a live session, so
    /// no unit test in this workspace can reach that statement, and this repo has six separately
    /// measured evasions of `include_str!` source pins — a pin here would assert the line is
    /// *written*, which is not the property at issue. The honest guard for "the publisher actually
    /// executes" is the live observation on a running client, and it is a check a human runs, not
    /// one the suite runs. Treat that call site as unguarded.
    ///
    /// ```compile_fail
    /// // The measured mutation, denied. A single-field write no longer type-checks.
    /// let mut v = eqoxide_ipc::ControllerView::default();
    /// v.hold = None;
    /// ```
    ///
    /// ```compile_fail
    /// // …and neither does the other half on its own.
    /// let mut v = eqoxide_ipc::ControllerView::default();
    /// v.afloat_stall = None;
    /// ```
    pub fn publish_disclosures(
        &mut self,
        d: (Option<eqoxide_core::game_state::ControllerHold>, Option<eqoxide_core::afloat::AfloatStall>),
    ) {
        (self.hold, self.afloat_stall) = d;
    }

    /// Drop both disclosures because the geometry they describe has been dropped (#846 review B1).
    ///
    /// Not a publish and not a guess: it is the statement "whatever the render thread last told us
    /// about this body's predicament was computed in a zone we have left, so there is nothing
    /// current here until it publishes again". Both disclosures are about *collision geometry* — a
    /// hold names a recovery path that does not exist, a stall names an anchor in a coordinate
    /// frame — so a zone change invalidates both at once, which is why this takes neither argument
    /// nor a choice of field.
    ///
    /// **Why this exists at all**, since `GameState::begin_zone_in` already clears the two
    /// `GameState` fields: clearing the copy does not clear the source. `ActionLoop::stream_position`
    /// mirrors `disclosures()` into those fields unconditionally on EVERY net tick, so the departed
    /// zone's hold was measurably restored one tick after `begin_zone_in` cleared it (#846 round-1
    /// review, B1: `after begin_zone_in: hold=None` → `after ONE net tick:
    /// hold=Some(EmbeddedNoRecovery, 7.5)`) — the mirror faithfully re-manufacturing a stale claim
    /// precisely because it is faithful. The clear has to happen here, at the value the mirror
    /// reads, or it does not survive contact with the mirror.
    ///
    /// Call it through [`ControllerSlots::begin_zone_in`] rather than directly, so that a caller
    /// does not perform half of the act.
    ///
    /// That is **convention, not a guarantee**, and the earlier wording here ("so the `GameState`
    /// clear and this one cannot be separated") was simply false — flagged in #846's round-2 review.
    /// Both this method and `GameState::begin_zone_in` are `pub`; nothing stops a caller from
    /// running one without the other, and #846's own M10 mutation *demonstrates* the separation by
    /// making `run_zone_entry_handshake` do exactly that. `ControllerSlots::begin_zone_in`'s doc
    /// says the same thing correctly, and the reason it cannot be a guarantee is structural:
    /// `eqoxide-core` sits below this crate, so `GameState::begin_zone_in` has to stay `pub` and
    /// separately reachable. What the pairing buys is one name for the whole act and a call-site
    /// test per existing caller — not unrepresentability.
    pub fn invalidate_disclosures(&mut self) {
        self.publish_disclosures((None, None));
    }
}

/// Which mode the orbit/follow camera is in. Relocated from `camera_state` (#544 Step 2c).
/// Serialized to the `/v1/camera` JSON — `rename_all = "snake_case"` is part of that wire form.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraMode { AutoFollow, ManualOrbit }

/// An HTTP `/camera` command the render loop applies to the `camera_state::CameraState`. Relocated
/// from `camera_state` (#544 Step 2c).
#[derive(Debug, Clone)]
pub enum CameraCmd {
    Set {
        azimuth:   Option<f32>,
        elevation: Option<f32>,
        radius:    Option<f32>,
        focus:     Option<[f32; 3]>,
    },
    Reset,
}

/// Snapshot of the current camera state for the HTTP GET `/camera` response. Relocated from
/// `camera_state` (#544 Step 2c); `camera_state::CameraState::snapshot` produces it. Serde form
/// preserved verbatim (it is the JSON body).
///
/// # Every field here describes ONE frame — the one named by `drawn_frame` — not "now" (#867)
///
/// The whole struct is published in a single write, and (#867) that write happens only after
/// `render_frame` has actually encoded a frame. `app.rs`'s render tick can `return` before the draw
/// on three `wgpu::SurfaceError` arms (`Lost`/`Outdated`/`Timeout`), and on those ticks **nothing is
/// published at all** — the previous snapshot stays. So on a skipped tick every field below is
/// stale, including the four "desired framing" ones, which can already have been mutated by an
/// `apply_cmd` earlier in that same tick.
///
/// **The staleness is not bounded.** `about_to_wait` only requests a redraw while
/// `now < active_until` (`ACTIVE_LINGER` = 300 ms), so a surface that keeps returning
/// `Outdated`/`Timeout` — a minimised or occluded window, not just a resize — plus 300 ms of quiet
/// stops the loop calling `render_frame` at all, and this snapshot then holds indefinitely. Do not
/// read "lag" as "a tick or two": read `drawn_frame`/`drawn_age_ms` and decide.
///
/// **Do not use the `snapshot_age_ms` on `/v1/observe/debug` to age this.** That is the *network*
/// health clock (`NetHealth::last_tick`) and stays fresh while a rendering stall makes every field
/// here arbitrarily old.
///
/// ## The desired side
///
/// `azimuth`/`elevation`/`radius`/`focus` are the orbit's DESIRED framing as of the frame named by
/// `drawn_frame` — the parameters a `/v1/camera Set` would reproduce. They are NOT necessarily the
/// eye position that frame was rendered from: in tight geometry the render loop pulls the eye in
/// along the focus→eye segment until it clears collision (`camera_state::resolve_camera_eye`), and
/// that pull-in does not touch these fields. A consumer that only reads `radius`/`focus` and
/// reconstructs a "distance to eye" by hand gets the *desired* distance, not the rendered one — use
/// `eye` for anything about what was actually drawn.
///
/// ## The rendered side
///
/// `eye`/`occluded`/`still_blocked` are the RENDERED side of the contract (#852). The render call
/// and this struct are built from the same `resolve_camera_eye` return value, never two
/// independently computed positions.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CameraSnapshot {
    /// Camera mode as of the frame named by `drawn_frame`. See `azimuth`.
    pub mode:          CameraMode,
    /// Desired orbit azimuth **as of the frame named by `drawn_frame`** — not necessarily as of
    /// now, and not necessarily the last value a `/v1/camera` Set wrote (see the struct doc).
    pub azimuth:       f32,
    /// Desired orbit elevation as of the frame named by `drawn_frame`. See `azimuth`.
    pub elevation:     f32,
    /// Desired orbit radius as of the frame named by `drawn_frame`. See `azimuth`.
    pub radius:        f32,
    /// Desired orbit focus point as of the frame named by `drawn_frame`. See `azimuth`.
    pub focus:         [f32; 3],
    /// The eye position the frame named by `drawn_frame` was rendered from (#852).
    ///
    /// When `drawn_frame` is `Some`, this describes a frame that was really encoded: the write is
    /// gated on an `eqoxide_renderer::DrawnFrame` token that only `render_frame` can produce, so it
    /// is not reachable on a tick whose draw was skipped. When `drawn_frame` is `None`, **no frame
    /// has been drawn yet** and this is the startup seed `main.rs` publishes before the event loop
    /// exists — a plausible-looking orbit position that nothing was ever rendered from.
    pub eye:           [f32; 3],
    /// True iff collision pulled `eye` in from the desired orbit position **on the frame named by
    /// `drawn_frame`** (not necessarily on the current tick — see the struct doc).
    pub occluded:      bool,
    /// True iff, even after the pull-in's iteration budget, `eye` was still not clear of collision
    /// along the segment to `focus` **on the frame named by `drawn_frame`** — a degenerate case
    /// (see `camera_state::resolve_camera_eye`).
    pub still_blocked: bool,
    /// Monotonic index of the frame every other field describes (#867), or `None` if **no frame has
    /// been drawn yet** — the startup seed. This is the freshness signal: without it a caller cannot
    /// distinguish a snapshot published this tick from one frozen since the window was minimised,
    /// because every other field looks identical in both cases. Poll it: unchanged across two reads
    /// means nothing has been drawn in between.
    pub drawn_frame:   Option<u64>,
    /// When that frame was drawn. Serialized as **`drawn_age_ms`** — milliseconds elapsed at the
    /// moment the response is encoded, or `null` when `drawn_frame` is `None`. `Instant` itself is
    /// never serialized; an absolute stamp would be meaningless to a remote reader and an age
    /// computed at publish time would itself go stale, so it is computed at read time.
    #[serde(rename = "drawn_age_ms", serialize_with = "serialize_age_ms")]
    pub drawn_at:      Option<std::time::Instant>,
}

/// Serialize an `Option<Instant>` as an age in whole milliseconds, measured when the response body
/// is encoded. See [`CameraSnapshot::drawn_at`].
fn serialize_age_ms<S: serde::Serializer>(
    v: &Option<std::time::Instant>,
    s: S,
) -> Result<S::Ok, S::Error> {
    match v {
        Some(t) => s.serialize_some(&(t.elapsed().as_millis() as u64)),
        None    => s.serialize_none(),
    }
}

/// Camera azimuth that places the camera behind a player facing `heading_deg`
/// (EQ convention: 0=north/+Y, CCW). Camera sits opposite the facing direction:
///   az = heading_rad - π/2
///
/// Relocated from `camera_state` (#422): `eqoxide-http`'s `GET /v1/observe/frame` preset resolver
/// (`observe::resolve_camera_override`) needs this exact formula to turn a preset/yaw override into
/// an azimuth relative to the character's current heading, but `eqoxide-http` cannot depend on the
/// `eqoxide` binary crate that owns `camera_state` — moving the (tiny, pure) formula here instead of
/// duplicating it keeps the two crates' notion of "behind the character" from ever drifting apart.
/// Re-exported from `camera_state::desired_azimuth` so every existing call site there is unchanged.
pub fn desired_azimuth(heading_deg: f32) -> f32 {
    heading_deg.to_radians() - std::f32::consts::FRAC_PI_2
}

/// Smoothed per-phase timings (milliseconds) for the HUD overlay. All zero until the first profiled
/// frame. Each field is an exponential moving average so the on-screen numbers are readable rather
/// than flickering frame-to-frame.
///
/// Relocated from `profiling` (#544 Step 2c). Serialized to `/v1/observe/debug` (`frame_profile`) —
/// the serde form is part of that wire contract. Its `blend` companion + the `FrameSample` it reads
/// moved with it (an inherent impl must be co-located with its type); the `Stopwatch` collection
/// helper stayed in `profiling`.
#[derive(Debug, Default, Clone, Copy, serde::Serialize)]
pub struct FrameProfile {
    pub update_ms: f32,
    /// Update sub-phase: rebuilding `SceneState` from `GameState` (per-frame snapshot clone).
    pub scene_ms:  f32,
    /// Update sub-phase: per-entity motion smoothing + floor snap.
    pub smooth_ms: f32,
    pub render_ms: f32,
    pub egui_ms:   f32,
    pub submit_ms: f32,
    pub total_ms:  f32,
    /// Instantaneous frames-per-second derived from `total` + idle wait (wall-clock between frames).
    pub frame_ms:  f32,
}

impl FrameProfile {
    /// Blend a fresh per-frame sample into the running average.
    pub fn blend(&mut self, s: &FrameSample, frame_ms: f32) {
        const A: f32 = 0.12; // EMA weight — ~0.5s settling at 60fps
        self.update_ms += (s.update_ms() - self.update_ms) * A;
        self.scene_ms  += (s.scene_ms()  - self.scene_ms)  * A;
        self.smooth_ms += (s.smooth_ms() - self.smooth_ms) * A;
        self.render_ms += (s.render_ms() - self.render_ms) * A;
        self.egui_ms   += (s.egui_ms()   - self.egui_ms)   * A;
        self.submit_ms += (s.submit_ms() - self.submit_ms) * A;
        self.total_ms  += (s.total_ms()  - self.total_ms)  * A;
        self.frame_ms  += (frame_ms      - self.frame_ms)  * A;
    }
}

/// Raw per-phase durations captured during one `render_frame`. Built only when profiling is enabled.
/// Relocated from `profiling` (#544 Step 2c) alongside `FrameProfile::blend`, which consumes it.
#[derive(Default)]
pub struct FrameSample {
    pub update: std::time::Duration,
    /// Sub-span of `update`: `SceneState::from_game_state`.
    pub scene:  std::time::Duration,
    /// Sub-span of `update`: entity motion smoothing + floor snap.
    pub smooth: std::time::Duration,
    pub render: std::time::Duration,
    pub egui:   std::time::Duration,
    pub submit: std::time::Duration,
    pub total:  std::time::Duration,
}

impl FrameSample {
    pub fn update_ms(&self) -> f32 { self.update.as_secs_f32() * 1000.0 }
    pub fn scene_ms(&self)  -> f32 { self.scene.as_secs_f32()  * 1000.0 }
    pub fn smooth_ms(&self) -> f32 { self.smooth.as_secs_f32() * 1000.0 }
    pub fn render_ms(&self) -> f32 { self.render.as_secs_f32() * 1000.0 }
    pub fn egui_ms(&self)   -> f32 { self.egui.as_secs_f32()   * 1000.0 }
    pub fn submit_ms(&self) -> f32 { self.submit.as_secs_f32() * 1000.0 }
    pub fn total_ms(&self)  -> f32 { self.total.as_secs_f32()  * 1000.0 }
}

/// The `--profile` / `EQ_PROFILE=1` on/off flag. Relocated from `profiling` (#544 Step 2o) — a
/// process-wide toggle read by both the app crate (`app::render_frame`'s phase timers) and
/// `eqoxide-ui` (gating its per-window timing log), so it lives beside the `FrameProfile`/
/// `FrameSample` data it gates rather than forcing either reader to depend on the other.
static PROFILING_ENABLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Turn frame profiling on/off (set once at startup from the `--profile` flag / `EQ_PROFILE` env).
pub fn set_enabled(on: bool) {
    PROFILING_ENABLED.store(on, std::sync::atomic::Ordering::Relaxed);
}

/// Whether the `--profile` overlay/timing is active.
#[inline]
pub fn enabled() -> bool {
    PROFILING_ENABLED.load(std::sync::atomic::Ordering::Relaxed)
}
// ── end relocated definitions ────────────────────────────────────────────────────────────────────

/// A STATELESS per-capture camera override for `GET /v1/observe/frame` (#422): applied ONLY to the
/// one off-screen render the render loop performs for this request, never written into the live
/// `camera_state::CameraState` the on-screen view uses — so a capture with an override can never
/// leave a later, override-less capture looking at a stale angle (the agent-honesty invariant this
/// crate's module doc references: no new observable that can get "stuck"). Fields mirror
/// `CameraSnapshot`'s `azimuth`/`elevation`/`radius` (radians / world units, same convention as
/// `camera_state::compute_eye`) — `eqoxide-http` resolves named presets and the degree-based
/// `pitch`/`yaw`/`distance` query params down to this triple before handing it across the channel,
/// so the render side needs no preset/unit knowledge at all.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CameraOverride {
    pub azimuth:   f32,
    pub elevation: f32,
    pub radius:    f32,
}

/// A pending frame capture: the render loop drains this, captures a PNG (optionally from an
/// off-screen render using `camera_override` instead of the live camera — #422), and sends the
/// bytes back through `tx`.
pub struct FrameCaptureRequest {
    /// `None` (the overwhelmingly common case, and always the case before #422) means: read back
    /// today's already-rendered on-screen frame, byte-for-byte the pre-#422 behavior. `Some` means:
    /// render one extra off-screen pass with this camera and read THAT back instead — the on-screen
    /// frame (and the live camera driving it) is untouched either way.
    pub camera_override: Option<CameraOverride>,
    pub tx: oneshot::Sender<Vec<u8>>,
}

/// A pending frame capture: the render loop drains this, captures a PNG,
/// and sends the bytes back through the channel.
pub type FrameReq = Arc<Mutex<Option<FrameCaptureRequest>>>;

/// A pending `/who all` request: GET /v1/observe/who registers a oneshot sender here; the nav thread
/// drains it, sends OP_WhoAllRequest, and fires it with the parsed roster when OP_WhoAllResponse
/// arrives. (#300)
pub type WhoReq = Arc<Mutex<Option<oneshot::Sender<Vec<eqoxide_core::game_state::WhoEntry>>>>>;

/// The client-local friends list (names). Edited by POST /v1/social/friends {add|remove}; read by the
/// nav thread to build the OP_FriendsWho poll and by GET /v1/social/friends to annotate online. (#301)
pub type FriendsListShared = Arc<Mutex<Vec<String>>>;
/// A pending friends-presence poll: GET /v1/social/friends registers a oneshot here; the nav thread
/// drains it, sends OP_FriendsWho, and fires it with the online-friends roster (the OP_WhoAllResponse
/// the server sends back) — mirrors [`WhoReq`]. (#301)
pub type FriendsReq = Arc<Mutex<Option<oneshot::Sender<Vec<eqoxide_core::game_state::WhoEntry>>>>>;

/// Target position for the navigation system. Set by /goto, cleared on arrival.
pub type GotoTarget = Arc<Mutex<Option<(f32, f32, f32)>>>;

/// When /goto targets a named ENTITY, this holds its `entity_positions` key so the nav walker can
/// re-resolve the entity's CURRENT position each tick and CHASE it — roaming mobs move (and their
/// client position is stale until they come within the server's update range), so pathing to a
/// one-time snapshot lands nowhere near them (eqoxide#88). `None` for coordinate gotos. Cleared
/// on arrival/stop alongside `goto_target`.
pub type GotoEntity = Arc<Mutex<Option<String>>>;

/// Authoritative controller snapshot published by the render thread each frame and read by the nav
/// thread to stream OP_ClientUpdate (design §2). Single source of position truth.
pub type ControllerShared = Arc<Mutex<ControllerView>>;

/// The `/goto` planner's per-frame movement intent. The nav planner writes `Some` while walking a
/// path and `None` when idle/arrived; the render controller consumes it when no WASD key is held.
pub type NavIntent = Arc<Mutex<Option<MoveIntent>>>;

/// A large (>12u) server position correction the nav thread hands to the render controller to apply
/// (teleport). Small deltas are ignored — the controller is authoritative (design §3.4).
pub type PosCorrection = Arc<Mutex<Option<[f32; 3]>>>;

/// Single-owner GameState publication (see
/// docs/superpowers/plans/2026-07-12-gamestate-single-owner-snapshot.md). The network thread is
/// the sole writer of `GameState`; it publishes an immutable clone here after every gameplay tick
/// via `eq_net::gameplay::publish_snapshot`. Render/HTTP consumers read it lock-free via `.load()`
/// (borrowed) or `.load_full()` (owned `Arc<GameState>`).
pub type GameStateSnapshot = std::sync::Arc<arc_swap::ArcSwap<eqoxide_core::game_state::GameState>>;

/// Why the current UDP session is over, when the client has POSITIVELY OBSERVED its end (#642).
///
/// This is distinct from the SILENCE-based `connected`/`last_datagram` signal: those go stale only
/// after [`CONN_STALE_SECS`] of no datagram, whereas each variant here is an *immediate, explicit*
/// server-side drop the client saw on the wire. It exists so a dropped session cannot masquerade as
/// a healthy one for the up-to-15s window before link silence would have caught it — once
/// `NetHealth::session_drop` is `Some`, `Health::connected` is forced `false` (see the derivation in
/// `HttpState::health`), so "connected for a session the server already ended" is unrepresentable.
///
/// Set by `EqStream` on the drop path; cleared only by a fresh `OP_SessionResponse`
/// (`handle_session_response`), so a legitimate zone-handoff reconnect re-arms it correctly while a
/// genuine drop stays reported until a new session is actually established.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDropCause {
    /// Inbound `OP_SessionDisconnect` (0x05): the server explicitly closed our session.
    ServerDisconnect,
    /// Inbound `OP_OutOfSession` (0x1d): the server received a datagram for a session it no longer
    /// knows — i.e. it has already dropped us and is telling us so.
    OutOfSession,
    /// The connected UDP socket's `recv` returned a closed/errored result (e.g. `ECONNREFUSED` from
    /// an ICMP port-unreachable) — the server endpoint is gone. This is `poll_recv`'s documented
    /// "socket closed" return, which was previously discarded at every call site.
    SocketClosed,
}

impl SessionDropCause {
    /// Stable, machine-readable snake_case name for the HTTP surface (`/v1/observe/debug` →
    /// `session_drop`). Kept explicit rather than derived from `Serialize` so the wire string is
    /// controlled here and cannot drift if the variant is renamed.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionDropCause::ServerDisconnect => "server_disconnect",
            SessionDropCause::OutOfSession => "out_of_session",
            SessionDropCause::SocketClosed => "socket_closed",
        }
    }
}

/// Where a health projection reads **"now"** from when it turns [`NetHealth`]'s stamps into ages.
///
/// #760. Every age in the projection is `now - stamp`. In production `now` is the real monotonic
/// clock, which is correct and must stay that way — silence *is* the signal (#343). But a TEST
/// fixture stamps its clocks at construction and is then read some unbounded time later, so its
/// ages are a function of **how long the test took**, i.e. of machine load. That made
/// `combat::tests::cast_empty_gem_is_409_and_queues_nothing` answer `503 stale session` instead of
/// the `409` it asserts whenever the box was busy enough to put >5s (`SESSION_STALE_TICK_MS`)
/// between `empty_state()` and the request being served — measured, not theorised: sleeping 5.1s
/// between the fixture and that request reproduces the 503 exactly.
///
/// Freezing the clock removes the wall clock from the test's answer entirely, rather than moving
/// the threshold it races against. A fixture built with [`NetHealth::frozen_at`] has
/// `now == stamp`, so every age it projects is **exactly** zero however long the test takes.
///
/// **A release build cannot construct a frozen clock.** The inner field is private *to this crate*
/// — production code inside `eqoxide-ipc`'s own `src/` could still write `HealthClock(Some(t))`
/// directly, which is the normal Rust module boundary; the guarantee is against every OTHER crate,
/// and it is measured, not reasoned (a `compile_error!` probe on the `test-fixtures` feature does
/// not fire in `cargo build --release --bin eqoxide`, and does fire under `cargo test`). The only
/// constructor that can set it from outside, [`HealthClock::frozen_at`], is behind the
/// `test-fixtures` feature —
/// so the frozen variant is *unrepresentable* outside a test build. That matters for the honesty
/// invariant in the other direction: a frozen health clock would pin `snapshot_age_ms` at 0 and
/// report a dead net thread as live forever, which is exactly the #343 lie this projection exists
/// to prevent.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HealthClock(Option<std::time::Instant>);

impl HealthClock {
    /// The real monotonic clock — the only clock a non-test build can have, and the `Default`.
    pub const WALL: HealthClock = HealthClock(None);

    /// A clock PINNED at `t`. Test fixtures only (see the type doc for why this is gated).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn frozen_at(t: std::time::Instant) -> Self {
        HealthClock(Some(t))
    }

    /// The instant every age in the projection is measured back from.
    pub fn now(self) -> std::time::Instant {
        self.0.unwrap_or_else(std::time::Instant::now)
    }

    /// True for a pinned clock. Lets a test assert its fixture really is load-independent instead
    /// of inferring it from a reading that happened to be 0.
    pub fn is_frozen(self) -> bool {
        self.0.is_some()
    }

    /// The age of `stamp` against this clock, saturating at zero for a stamp in the future (which a
    /// pinned clock makes reachable: a test may re-stamp a field after freezing).
    pub fn age_of(self, stamp: std::time::Instant) -> std::time::Duration {
        self.now().saturating_duration_since(stamp)
    }

    /// A stamp `secs` behind this clock's own reading of now — the inverse of [`age_of`].
    ///
    /// On a **pinned** clock the round trip is exact: `age_of(ago(n)) == n`. On the **wall** clock it
    /// is not, and cannot be — `now` advances between minting and reading, so `age_of(ago(n)) >= n`
    /// is the strongest true statement. Both are asserted in
    /// `ago_is_the_inverse_of_age_of_on_the_same_clock_and_drifts_across_clocks`.
    ///
    /// Every past-dated net-health stamp in a test must come from here rather than from a bare
    /// `Instant::now() - secs`. On a wall clock the two are the same expression; on a PINNED clock
    /// they are not, and the difference is a silent, drifting error:
    /// `now() - secs` read back against a clock pinned at fixture construction ages to
    /// `secs − (time since construction)`, so any assertion that needs the stamp to stay ABOVE a
    /// bound has a margin that machine load eats. That is #760's own failure mode, re-armed one
    /// level down — it happened, in review, to `debug_reports_world_unresponsive_when_a_probe_goes_
    /// unanswered_while_the_link_acks` (a 15s stamp that had to clear a 10s bound: a 5s margin).
    /// Deriving the stamp from the clock that will read it removes that drift, so the correct call
    /// has no wrong variant to choose between.
    ///
    /// [`age_of`]: HealthClock::age_of
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn ago(self, secs: u64) -> std::time::Instant {
        self.now()
            .checked_sub(std::time::Duration::from_secs(secs))
            .expect("monotonic clock younger than the requested stamp age")
    }
}

/// The three clocks that answer "can I trust anything else in this payload?", owned and stamped by
/// the network thread and turned into `Health` **at HTTP read time** (`HttpState::health`), never
/// cached (#8, #343). They are deliberately separate signals, because they fail independently:
///
/// | clock           | bumped when                        | a stale value means                      |
/// |-----------------|------------------------------------|------------------------------------------|
/// | `last_datagram` | ANY inbound UDP datagram           | **the link is dead** → `connected: false` |
/// | `last_packet`   | an inbound APPLICATION packet       | the world is quiet (NOT necessarily dead) |
/// | `last_tick`     | every gameplay tick (~10ms)         | OUR network thread wedged/died            |
///
/// The `last_datagram` / `last_packet` split is load-bearing and was found by live-testing #343: a
/// genuinely idle EQ session (a character sitting alone in an empty zone) goes **40+ seconds**
/// without a single application packet, while the session layer keeps ACKing throughout. Deriving
/// `connected` from application traffic would therefore report a perfectly healthy idle session as
/// disconnected — trading the old false `true` for an equally dishonest false `false`.
///
/// #371 adds a FOURTH failure those three cannot fully see: a zone that is **still ticking but not
/// making application progress for us** — a stuck per-client dispatch, an infinite/blocking quest
/// script, or a tick so slow it never services our packets. Such a zone keeps ACKing
/// (`last_datagram` fresh → `connected: true`) while producing no application output for us
/// (`last_packet` climbing) — which is *pixel-identical* to a healthy-but-idle zone, because the
/// symptom is exactly "the world stopped speaking". No passive clock can separate them. The only
/// sound discriminator is an ACTIVE probe: periodically send a cheap request the zone MAIN LOOP
/// must service to answer, and time the reply. `last_probe_sent` / `last_probe_reply` are that
/// round-trip's clocks; `HttpState::health` turns them into `world_responsive` at read time.
///
/// SCOPE (do not oversell): this EQEmu build runs the zone as a single-threaded libuv loop, so a
/// *total* freeze stops the ACKs too and is ALREADY caught by `connected: false`. `world_responsive`
/// does NOT add total-freeze detection — it adds the still-ticking-but-unresponsive case above,
/// which `connected` cannot see. (The old Titanium `EQStreamFactory` split a hung main loop from a
/// still-ACKing reader thread; this server does not work that way — do not reason from that model.)
#[derive(Debug, Clone, Copy)]
pub struct NetHealth {
    /// Where the health projection reads "now" from (#760). `HealthClock::WALL` in every build that
    /// is not a test; see [`HealthClock`].
    pub clock: HealthClock,
    /// Last inbound datagram of ANY kind, session-layer ACKs/keepalives included → link liveness.
    pub last_datagram: std::time::Instant,
    /// Last inbound APPLICATION packet (a decoded opcode that mutated `GameState`) → world activity.
    /// NOTE: the #371 liveness-probe reply is deliberately NOT stamped here — it is a solicited poke,
    /// not spontaneous world output, and counting it would cap `last_packet_age_ms` at the probe
    /// cadence and destroy its "the world has been quiet for 45s" meaning. It stamps `last_probe_reply`.
    pub last_packet:   std::time::Instant,
    /// Last network-thread gameplay tick → client liveness (is our own publisher still running?).
    pub last_tick:     std::time::Instant,
    /// When the network thread MOST RECENTLY (re)sent an active liveness probe (#371). Bumped on
    /// every 30s resend while a probe stays unanswered — this is a scheduling clock only. Do NOT feed
    /// this into `world_responsive`'s timeout check: resending an already-unanswered probe must not
    /// look like a *fresh* one, or a permanently wedged zone would flicker back to "responsive" every
    /// time the resend fires (the exact bug this comment is warning against — see
    /// `first_unanswered_probe_sent` below, which is what `world_responsive` actually reads).
    /// `None` until the first probe fires (e.g. before we are fully in-zone) — in which case there is
    /// simply no probe verdict yet and `world_responsive` defers to the passive signals.
    pub last_probe_sent:  Option<std::time::Instant>,
    /// When we last saw the probe's reply come back from the zone (#371). Compared against
    /// `first_unanswered_probe_sent` to tell an answered probe from an outstanding one.
    pub last_probe_reply: Option<std::time::Instant>,
    /// When the CURRENT unanswered-probe streak began (#371 wedge-flicker fix). Set the first time a
    /// probe is sent while none is already outstanding; deliberately left UNCHANGED by later resends
    /// of that same still-unanswered probe, so a zone that never answers cannot "earn" a fresh 10s
    /// in-flight grace window every time we poke it again. Reset to `None` the moment ANY proof of
    /// life arrives — a genuine probe reply (`record_probe_reply`) OR any spontaneous application
    /// packet (`record_app_packet`) — and on zone-change (`reset_probe_clocks`). Clearing on
    /// spontaneous traffic is load-bearing: it re-arms the clock so a SECOND wedge after a traffic
    /// recovery is timed freshly and still detected (without it, a stale streak-start would make the
    /// answered-clause permanently true → a confident false-alive). This — not `last_probe_sent` — is
    /// what `world_responsive` measures its timeout against, so once a wedge verdict is reached within
    /// one continuous silence it stays `false` until real proof of life, no matter how many resends
    /// happen in between.
    pub first_unanswered_probe_sent: Option<std::time::Instant>,

    // ── Outbound send failures (#612) ──────────────────────────────────────────────────────────
    //
    // Every clock above is about what the SERVER did. These four are about what WE failed to do:
    // a datagram the client built but that never left the machine, because `try_send` returned an
    // error (`WouldBlock`, `ENOBUFS`, `EMSGSIZE`, `ENETUNREACH`, a dead socket…). Before #612 that
    // error was discarded (`let _ = self.socket.try_send(&raw)`), so a packet that never reached
    // the wire was indistinguishable from one that did — the agent-honesty failure the invariant
    // exists to prevent, one layer below #513/#347. `EqStream::transmit` is now the ONLY place in
    // the client that touches the socket's send path, and it stamps these on every failure, so a
    // send cannot fail without being counted.
    /// Cumulative count of outbound datagrams whose `try_send` failed — i.e. that were BUILT but
    /// never put on the wire. Since process start; never reset (a zone change does not un-drop a
    /// packet).
    ///
    /// **`0` IS the expected healthy reading since #641.** History, because the previous text here
    /// said the opposite and an agent reading it would have learned to ignore this counter: the
    /// #612 round-2 review measured **283** on a fresh, healthy login into `qeynos` — all
    /// `WouldBlock`, all 7-byte session-layer control datagrams (ACKs), in a burst during zone-in
    /// and then flat. #641 gave those two recovery paths (an immediate direct `send(2)` retry, and
    /// a deferral queue for control datagrams), and both are counted elsewhere —
    /// `send_wouldblock_rescued` and `send_deferred`. So this counter now means what its name says:
    /// the datagram never reached the wire, and nothing will re-send it.
    ///
    /// The TRIGGER is established — CPU starvation of the client's tokio io driver, reproducible by
    /// pinning the client to one core. The MECHANISM is not: see `send_wouldblock_rescued` for why
    /// neither counter can tell a tokio-synthetic refusal from a kernel one.
    pub send_failures: u64,
    /// Datagrams whose `try_send` returned `WouldBlock` and which an immediate direct `send(2)` on
    /// the same fd then ACCEPTED (#641). They reached the wire, which is why they are counted here
    /// and not in `send_failures`.
    ///
    /// **This is an UPPER BOUND on tokio's synthetic-`WouldBlock` case, not a measurement of it**
    /// (#641 review, finding 3). Two mechanisms produce the same `WouldBlock`:
    ///   1. tokio short-circuits on an empty cached readiness bit and returns `WouldBlock` *without
    ///      issuing the syscall* (the bit is refilled only by its io driver); or
    ///   2. the bit is set, the syscall IS issued, and the kernel returns `EAGAIN`/`ENOBUFS` (which
    ///      also clears the bit).
    /// A direct `send(2)` succeeding microseconds later fits (1) — but fits (2)-then-the-buffer-
    /// drained just as well, and a burst is exactly when the buffer is full and draining hard. So
    /// the error is systematic in one direction. A DOUBLE refusal (the direct `send(2)` fails too)
    /// is hard evidence of (2); that is what refutes "it is all synthetic", and it is all that is
    /// established. Telling them apart properly would need something like `ioctl(SIOCOUTQ)` at the
    /// moment of refusal (≈0 queued bytes ⇒ genuinely synthetic); nobody has done that.
    ///
    /// Read it as a LOAD signal — the socket is refusing sends here — not as a diagnosis. The split
    /// varies RUN TO RUN, not by zone: `gfaydark` measured 0 rescued / 138 deferred on one run and
    /// 175 / 147 on another, same recipe and same binary; `qeynos` measured 141/107, 166/106 and
    /// 119/114. Nothing observable predicts it.
    ///
    /// Before #641 every one of these was a datagram silently dropped on the floor — mostly ACKs,
    /// which the server then had to re-solicit by retransmitting the packets it had not seen
    /// acknowledged.
    pub send_wouldblock_rescued: u64,
    /// How many **datagrams** a transient send refusal (`EAGAIN`/`ENOBUFS`) caused to be QUEUED for
    /// retry on a later net-thread tick instead of being dropped (#641). Only session-layer control
    /// is deferrable — ACK / OutOfOrderAck / keepalive / SessionRequest. (`SessionDisconnect` is
    /// deliberately NOT: there is no "next tick" at shutdown. See `send_session_disconnect`.)
    ///
    /// **Datagrams, not refusal events.** Counted exactly once, in `defer_control`, at the moment
    /// the datagram is queued. It is *not* incremented again when a queued datagram is re-attempted
    /// and refused again, so the number tracks how many datagrams were delayed, not how long the
    /// outage lasted. The first cut of #641 got this wrong in the other direction and its docs and
    /// its code disagreed (#641 review, finding 1).
    ///
    /// **Not a loss counter, and NOT disjoint from `send_failures`** (#641 review, finding 1b). In
    /// the normal case each of these datagrams goes out on a later tick, ~10ms late. But a deferred
    /// datagram can still be lost afterwards — the queue overflows, or the session ends while it is
    /// still queued — and that loss is counted in `send_failures`/`send_failures_unretried` too, so
    /// the same datagram appears in both. `send_failures` stays the honest "was anything lost?"
    /// number; this one answers "how many datagrams did the socket make us delay?".
    ///
    /// That holds on EVERY path that ends a session, including the `OP_GMKick` one that parks
    /// forever without ever unwinding: it calls `abandon_outstanding` explicitly (#641 review R3),
    /// because a `Drop` that never runs cannot account for anything. A counter that is honest
    /// "except on one path" decays into a counter nobody trusts.
    ///
    /// A lower bound on genuine kernel refusals, for the reason given on `send_wouldblock_rescued`.
    /// Before #641 every one of these was a silently dropped ACK, which the server answered by
    /// retransmitting everything it had not seen acknowledged — the road to a `resend_timeout` drop.
    pub send_deferred: u64,

    // ── #656: is the io driver starved RIGHT NOW? ──────────────────────────────────────────────
    //
    // #641 gave the client `send_wouldblock_rescued`/`send_deferred`, but both are cumulative since
    // process start — they can only grow, so nothing could ever tell "the driver was starved once,
    // an hour ago" from "it is starved right now". Nothing consumed them either: no HUD, no WARN,
    // no documented threshold (#656). These two fields turn the pair into a RATE signal (via
    // `send_starved`, below) instead of a lifetime count, which is what lets the derived alert
    // CLEAR on its own once a burst ends.
    /// When the most recent `send_wouldblock_rescued` OR `send_deferred` event happened. `None`
    /// until the first one this process has ever seen. Stamped by `record_send_pressure`, which is
    /// called from both increment sites (the rescue in `EqStream::attempt_send` and the queue in
    /// `EqStream::defer_control`) — never write these two fields directly from anywhere else, or a
    /// stale value here would let the derived alert stay wrongly cleared forever, the honesty
    /// failure this exists to prevent.
    pub last_send_pressure_at: Option<std::time::Instant>,
    /// Length of the CURRENT consecutive run of `send_wouldblock_rescued`/`send_deferred` events,
    /// where "consecutive" means each arrived within [`SEND_PRESSURE_BURST_GAP_SECS`] of the one
    /// before it. Reset to `1` (not incremented) the first time an event arrives after a longer
    /// gap — see `record_send_pressure`. This is what filters out the single stray `WouldBlock` on a
    /// freshly `connect()`ed socket (#603/#610, a documented harmless one-off, not CPU pressure)
    /// from a genuine sustained refusal run.
    pub send_pressure_streak: u64,
    /// The subset of `send_failures` for datagrams the client does **not** retransmit itself:
    /// unreliable app packets (the `OP_ClientUpdate` position firehose), session-layer control
    /// (ACK / OutOfOrderAck / keepalive / SessionRequest / SessionDisconnect). The complement
    /// (`send_failures - send_failures_unretried`) is the reliable stream, where the failed
    /// datagram is retained verbatim in the resend window and re-sent by `poll_resend` until the
    /// server ACKs it — **for as long as the session lives**.
    ///
    /// That qualifier is load-bearing (#612 review F1) and this counter must NOT be read as a
    /// complete count of lost payload: when a session ends while reliables are still outstanding,
    /// the next stream's window starts EMPTY and those datagrams are genuinely lost while this
    /// counter reads 0 for all of them.
    ///
    /// **Two different endings, and which counter sees them:**
    ///   - A zone handoff / world reconnect / clean shutdown — counted by `reliable_abandoned`.
    ///   - A server-side drop the client OBSERVES (inbound `OP_SessionDisconnect`/`OP_OutOfSession`,
    ///     or a closed socket) — since #642 this tears the gameplay phase down, dropping the stream,
    ///     so `reliable_abandoned` counts it too, and `session_drop` names the cause.
    ///   - **A server drop into TOTAL silence (no disconnect, no OutOfSession, no ICMP) — counted by
    ///     NOTHING.** No `session_drop` is set, so the stream is not torn down and `reliable_abandoned`
    ///     does not rise. `connected: false` (15s of link silence) is the only signal for this
    ///     residual sub-case.
    ///
    /// This paragraph has now regenerated the wrong way four times across #612's reviews — most
    /// recently right here, under a field whose name does not contain "abandoned", which is exactly
    /// why greps keyed on `reliable_abandoned` kept missing it. `docs/http-api.md` and
    /// `eqoxide_http::Health` both point readers HERE for the coverage list, so if this doc is wrong
    /// the whole chain is. If you edit it, grep `resend_timeout` across the workspace, not this
    /// field's neighbourhood.
    ///
    /// Do NOT read a nonzero value here as "a command was lost": several of these datagrams have a
    /// recovery path one level up (a fresh position update follows ~50ms later; a lost ACK is
    /// re-solicited by the server's own resend). It means "this exact datagram is gone, and the
    /// client will not re-send THAT datagram" — which is a real, previously invisible fact.
    pub send_failures_unretried: u64,
    /// `ErrorKind` of the most recent send failure (`None` if there has never been one). Kept as an
    /// `ErrorKind` rather than a `String` so `NetHealth` stays `Copy` (it is read by value under the
    /// mutex, like every other field here).
    pub last_send_error_kind: Option<std::io::ErrorKind>,
    /// When the most recent send failure happened. Measured into an age at HTTP READ time, never
    /// stored as a duration — same rule as every other clock in this struct (#343).
    pub last_send_error_at: Option<std::time::Instant>,
    /// Un-ACKed RELIABLE datagrams that were abandoned when a session ended (#612, review F1).
    ///
    /// `send_failures_unretried` deliberately excludes the reliable stream, because `poll_resend`
    /// re-sends a failed reliable datagram verbatim until the server ACKs it. That guarantee holds
    /// only **while the session lives**. EQEmu drops the session at its ~30s `resend_timeout`, and
    /// the reconnect builds a FRESH `EqStream` whose resend window starts EMPTY — every datagram
    /// still outstanding at that moment is genuinely lost, and no amount of "it will be
    /// retransmitted" is true of it any more.
    ///
    /// Without this counter that loss would be exactly the bug #612 fixed, one level up: a
    /// documented contract telling the agent a class of loss cannot have happened when it can.
    /// `EqStream`'s `Drop` impl adds its outstanding window here, so every path that TEARS THE
    /// STREAM DOWN is counted without each one remembering to mirror it. See the COVERAGE note
    /// below for the paths that do not tear it down — one of them is not covered at all.
    ///
    /// Note this counts abandonment, not necessarily *loss of an unsent packet*: a datagram that
    /// reached the wire and whose ACK simply had not arrived yet when we handed off is also counted.
    /// It is an upper bound on "reliable payload this client stopped trying to deliver", which is
    /// the honest direction to err in.
    ///
    /// **MEASURED (#612 round 2): three consecutive clean zone handoffs (qeynos → qeytoqrg → qeynos
    /// → freportw) left this at 0, with zero abandonment WARNs** — the resend window was empty at
    /// every handoff. An earlier version of this doc predicted, from reasoning and explicitly
    /// unmeasured, that a clean handoff "routinely leaves a small number"; that was WRONG and would
    /// have trained an agent to ignore the counter's most likely true positive. **Treat a nonzero
    /// value DURING PLAY as signal, not noise.**
    ///
    /// **Clean shutdown is the one measured exception, and it is expected to be nonzero.** Two live
    /// `/v1/lifecycle/exit` runs measured 4 and 8 (#612 round 3/4). It is invisible to an agent
    /// either way — the process is exiting — so scope the "0 is normal" reading to play, not to exit.
    ///
    /// **The CAUSE of that count is NOT established.** What is known structurally: `OP_Logout` is a
    /// single reliable datagram, so it can account for at most 1; and `OP_SessionDisconnect` cannot
    /// contribute at all, because it is framed by `send_raw` (`SendRetry::None`) and the only
    /// `self.sent.push_back` in the client is in `send_tracked`. What is known empirically: the two
    /// runs INVERT the naive prediction — 4 with reliable traffic injected, 8 on a control run with
    /// none. An earlier version of this doc asserted the "closing OP_Logout/SessionDisconnect are
    /// still un-ACKed" mechanism; it was wrong on both counts and is withdrawn. The remaining count
    /// is most likely reliables left over from earlier in the session, but that is a HYPOTHESIS,
    /// not a traced fact — do not repeat it as one.
    ///
    /// **COVERAGE — read this before relying on a 0.** It is written where the abandonment can be
    /// observed, which is not everywhere a session can end:
    ///   - **Covered:** zone handoff and world reconnect (both `drop` the old stream), zone-in
    ///     failure returns, and clean shutdown (which calls `abandon_outstanding` explicitly,
    ///     because its task parks and is never unwound).
    ///   - **Covered since #642: an OBSERVED server-side session drop** — inbound
    ///     `OP_SessionDisconnect`/`OP_OutOfSession`, or a closed socket. These now set `session_drop`
    ///     and the gameplay loop tears the phase down, dropping the stream, so this counter finally
    ///     rises for the ~30s `resend_timeout` case when the server signals it.
    ///   - **NOT covered: a server drop into TOTAL silence** — no disconnect, no OutOfSession, no
    ///     ICMP. Nothing sets `session_drop`, so the stream is not dropped and this stays 0 there.
    ///     `connected: false` (15s of link silence) remains the signal for that residual sub-case —
    ///     not this counter.
    pub reliable_abandoned: u64,
    /// #642: `Some` once the client POSITIVELY OBSERVES that the server ended this session — inbound
    /// `OP_SessionDisconnect`/`OP_OutOfSession`, or a closed/errored socket recv. Before #642 all
    /// three signals were discarded (`dispatch_transport`'s `_ => {}`, `poll_recv`'s ignored bool),
    /// so a server-side session drop left the client looping on a dead stream while `/v1/observe`
    /// still reported a live session for up to `CONN_STALE_SECS`. This turns that into an immediate,
    /// explicit terminal state: it FORCES `Health::connected` false (so a dropped socket can never
    /// read connected) and drives the gameplay loop's honest teardown, which drops the stream and
    /// finally makes `reliable_abandoned` cover this case. `None` while a session is live; set on the
    /// drop path and cleared by a fresh `OP_SessionResponse` (`handle_session_response`), so a
    /// zone-handoff reconnect re-arms it. See [`SessionDropCause`].
    pub session_drop: Option<SessionDropCause>,
}

impl Default for NetHealth {
    fn default() -> Self {
        let now = std::time::Instant::now();
        NetHealth {
            clock: HealthClock::WALL,
            last_datagram: now, last_packet: now, last_tick: now,
            last_probe_sent: None, last_probe_reply: None,
            first_unanswered_probe_sent: None,
            send_failures: 0, send_wouldblock_rescued: 0, send_deferred: 0,
            last_send_pressure_at: None, send_pressure_streak: 0,
            send_failures_unretried: 0,
            last_send_error_kind: None, last_send_error_at: None,
            reliable_abandoned: 0,
            session_drop: None,
        }
    }
}

impl NetHealth {
    /// #760: a fixture whose clock is PINNED at `now`, with `last_datagram`/`last_packet`/`last_tick`
    /// stamped at that same instant *minus* the ages the caller asks for (in seconds). Every age the
    /// health projection derives from it is then **exactly** the number passed in — not "that number
    /// plus however long the test has been running so far" — so a test's liveness verdict cannot
    /// depend on machine load. `(0, 0, 0)` is a perfectly live session, permanently.
    ///
    /// Test fixtures only; see [`HealthClock`] for why a release build cannot reach this.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn frozen_at(
        now: std::time::Instant,
        datagram_ago_secs: u64,
        tick_ago_secs: u64,
        packet_ago_secs: u64,
    ) -> Self {
        // `checked_sub` + `expect` (the same shape as `eqoxide_http::testkit::ago`): on a host whose
        // monotonic epoch is younger than the age asked for there is no instant to name, and a loud
        // panic naming the cause beats silently clamping to the epoch and reporting a smaller age
        // than the fixture asked for.
        let back = |secs: u64| {
            now.checked_sub(std::time::Duration::from_secs(secs))
                .expect("monotonic clock younger than the requested fixture age")
        };
        NetHealth {
            clock: HealthClock::frozen_at(now),
            last_datagram: back(datagram_ago_secs),
            last_tick: back(tick_ago_secs),
            last_packet: back(packet_ago_secs),
            ..NetHealth::default()
        }
    }

    /// #656: stamp a `send_wouldblock_rescued`/`send_deferred` event and update the consecutive-
    /// burst streak that `send_starved` reads. Call this from BOTH increment sites (never bump
    /// `send_pressure_streak`/`last_send_pressure_at` any other way, or the two would drift from the
    /// counters they exist to summarize).
    ///
    /// A gap since the previous event longer than [`SEND_PRESSURE_BURST_GAP_SECS`] starts a NEW
    /// streak at `1`, rather than adding to the old one — this is what makes a single stray event
    /// long ago (e.g. the one documented, harmless `WouldBlock` on a freshly `connect()`ed socket,
    /// #603/#610) read as `streak == 1`, not as an ever-growing total that could eventually cross the
    /// fire threshold on its own.
    pub fn record_send_pressure(&mut self, now: std::time::Instant) {
        let same_burst = self.last_send_pressure_at
            .is_some_and(|t| now.saturating_duration_since(t)
                <= std::time::Duration::from_secs(SEND_PRESSURE_BURST_GAP_SECS));
        self.send_pressure_streak = if same_burst { self.send_pressure_streak.saturating_add(1) } else { 1 };
        self.last_send_pressure_at = Some(now);
    }
}

/// #371: a probe left unanswered longer than this — while no spontaneous application packet has
/// arrived either — means the zone main loop is not processing (a wedged world), even though the
/// link keeps ACKing. Kept below `PROBE_INTERVAL` so a wedge is declared before the next probe is
/// even due; kept well above a normal round-trip so ordinary latency never false-alarms.
pub const PROBE_TIMEOUT_SECS: u64 = 10;

/// #371 resend cadence for an unanswered liveness probe — and, crucially for #470, the interval
/// before the NEXT probe is due AFTER one is answered. `gameplay.rs`'s `PROBE_INTERVAL` is built from
/// this (single source of truth), and `PASSIVE_LIVENESS_STALE_SECS` is derived from it. See the note
/// on the passive bound below for why an ANSWERED probe re-enters the passive branch for a full
/// interval, which is what makes this value — not the first-probe timing — the one that matters.
pub const PROBE_INTERVAL_SECS: u64 = 30;

/// #470: passive-liveness staleness bound for the "no probe outstanding" branch of
/// [`world_responsive`]. It exists to condemn a ZOMBIE session whose active prober is DEAD, WITHOUT
/// ever false-condemning a healthy idle-but-answering session (#343).
///
/// The prober runs inside the gameplay net thread's loop (`gameplay.rs`). A failed world-reconnect
/// can leave that thread exited: no more probes are ever sent, so `first_unanswered_probe_sent`
/// stays `None` forever and the active-probe path can NEVER declare a wedge. Pre-#470 the `None`
/// branch returned `true` unconditionally, so a fully dead session reported `world_responsive: true`
/// indefinitely — the exact agent-honesty lie #470 is about. This bound lets the passive proof-of-life
/// clock ALONE condemn such a session even with no probe outstanding.
///
/// DERIVED FROM THE RESEND CADENCE, not the first-probe timing (the bug the first cut of #470 had).
/// A HEALTHY idle-but-answering session spends most of its life in the `None` branch: the instant a
/// probe is answered, `record_probe_reply` clears the unanswered streak back to `None`, and the NEXT
/// probe is not sent until `PROBE_INTERVAL_SECS` (30s) after the previous SEND. So its freshest
/// proof-of-life (`probe_reply_ago`) climbs to nearly a FULL interval before the next probe refreshes
/// it. The bound must therefore exceed one whole probe cycle plus its reply window, or a perfectly
/// healthy every-probe-answering session would be condemned for the tail of every cycle, forever
/// (`PROBE_INTERVAL` 30s + `PROBE_TIMEOUT` 10s = 40s; the earlier `PROBE_QUIET + PROBE_TIMEOUT` = 22s
/// was < 30 and did exactly that). At the same time a genuinely dead-but-connected session is still
/// condemned: a live prober would have re-probed at 30s and, getting no reply, moved to the Some/
/// timeout branch by ~40s anyway — so nothing alive is still sitting in the `None` branch past 40s.
pub const PASSIVE_LIVENESS_STALE_SECS: u64 = PROBE_INTERVAL_SECS + PROBE_TIMEOUT_SECS;

/// #371/#470: decide, at HTTP read time, whether the WORLD (not just the link) is alive, from the
/// link/probe/app clocks expressed as ages (time since the event; `None` = it never happened). Pure
/// so the state machine can be exhaustively unit-tested without a socket. Returns `world_responsive`.
///
/// **#470 link gate (checked first):** a dead LINK cannot host a responsive world, regardless of any
/// probe verdict. `connected == false` → `false`. This is the branch that actually bites the zombie
/// bug: a failed world-reconnect kills the net thread (and with it the prober), so no probe is ever
/// outstanding; the pre-#470 code then fell straight through to the unconditional `true` below. The
/// caller MUST pass the SAME `connected` it publishes in `Health` (derived from `last_datagram`).
///
/// With the link alive, a probe is only damning once it is BOTH unanswered AND overdue:
/// - **No probe outstanding** → defer to the passive `last_packet` clock. `true` while packets are
///   fresher than `passive_stale`; `false` once staler (#470 — a live prober would have fired and
///   moved us to the `Some` branch by then, so this staleness means the prober itself is gone). A
///   genuinely idle-but-answering session (#343) is in the `Some` branch by now and never reaches
///   here — see [`PASSIVE_LIVENESS_STALE_SECS`].
/// - **Answered** → `true`. "Answered" = proof the zone processed something at or after we sent the
///   probe: its own reply (`probe_reply_ago <= first_unanswered_sent_ago`) OR *any* spontaneous
///   application packet since (`last_packet_ago <= first_unanswered_sent_ago`). The second clause is
///   belt-and-suspenders: a busy zone is obviously alive even if a single probe reply was dropped,
///   and it is exactly what keeps a legitimately-quiet-but-answering idle session from ever
///   false-alarming.
/// - **Outstanding but not yet overdue** (`first_unanswered_sent_ago < timeout`) → `true`. Still in
///   flight; never mistake normal latency for a wedge.
/// - **Outstanding AND overdue** → `false`. The wedged-world signal — the whole point of #371.
///
/// CALLER CONTRACT (#371 wedge-flicker fix): `first_unanswered_sent_ago` MUST be the age of the
/// FIRST send of the current unanswered probe streak, not the most recent resend. `gameplay.rs`
/// resends an unanswered probe every `PROBE_INTERVAL` (30s) purely to keep detecting recovery; if
/// this function were fed the age of that most-recent resend instead, a permanently wedged zone
/// would re-enter the "still in flight" branch every 30s and flicker back to `true` forever even
/// though it never actually answers. `NetHealth::first_unanswered_probe_sent` is the clock that
/// holds still across resends and only clears on a genuine reply or a zone change — feed that one.
pub fn world_responsive(
    connected:                 bool,
    first_unanswered_sent_ago: Option<std::time::Duration>,
    probe_reply_ago:           Option<std::time::Duration>,
    last_packet_ago:           std::time::Duration,
    timeout:                   std::time::Duration,
    passive_stale:             std::time::Duration,
) -> bool {
    // #470 link gate: a dead link is a dead world, no matter what the probe clocks say (and in the
    // zombie case they say nothing — the prober died, leaving `first_unanswered_sent_ago == None`).
    if !connected {
        return false;
    }
    match first_unanswered_sent_ago {
        // No unanswered-probe streak, link alive. This state has TWO causes and they must be told
        // apart: (a) a probe was just ANSWERED — `record_probe_reply` clears the streak, leaving a
        // FRESH `probe_reply_ago` — a legitimately idle-but-answering session (#343) whose last
        // spontaneous packet may be tens of seconds stale yet is provably alive; or (b) the prober is
        // DEAD (#470) — no probe ever replied and no packet has arrived for the whole window. So
        // condemn only when the FRESHEST proof of life (spontaneous packet OR probe reply, exactly the
        // pair `last_world_response_ms` reports) is itself staler than the symmetric bound.
        None => {
            let proof_of_life_ago = probe_reply_ago.map_or(last_packet_ago, |r| r.min(last_packet_ago));
            proof_of_life_ago < passive_stale
        }
        Some(sent_ago) => {
            let answered = probe_reply_ago.is_some_and(|r| r <= sent_ago)
                        || last_packet_ago <= sent_ago;
            answered || sent_ago < timeout
        }
    }
}

/// #656: max gap between successive `send_wouldblock_rescued`/`send_deferred` events for them to
/// count as the SAME burst (see [`NetHealth::record_send_pressure`]). A gap larger than this ends
/// the burst — this is what lets [`send_starved`] CLEAR the instant no new pressure event has
/// arrived within this window, no matter how large the historical streak or the lifetime counters
/// (`send_wouldblock_rescued`/`send_deferred`) were. Kept short: these events are generated at the
/// ~10ms net-thread tick cadence during a real burst, so a genuine ongoing burst never lets this
/// gap open, while a burst that has actually ended clears within a couple of seconds of ending.
pub const SEND_PRESSURE_BURST_GAP_SECS: u64 = 2;

/// #656: how many consecutive-burst events (within [`SEND_PRESSURE_BURST_GAP_SECS`] of each other)
/// it takes before [`send_starved`] fires. #603/#610 documented that a single, isolated `WouldBlock`
/// on a freshly `connect()`ed socket is normal and harmless — NOT evidence of CPU pressure — so a
/// threshold of `1` would false-alarm on every ordinary session. #641 measured genuinely
/// CPU-starved zone-ins producing bursts of 100-200; this is set low enough to catch the ONSET of a
/// real burst rather than wait for it to finish, while still requiring more than one stray event.
pub const SEND_PRESSURE_FIRE_THRESHOLD: u64 = 5;

/// #656: is the client's outbound send path starved RIGHT NOW — i.e. is a
/// `send_wouldblock_rescued`/`send_deferred` burst happening currently, as opposed to having
/// happened at some point in the past? Pure so the fire/clear boundary is unit-testable without a
/// socket or a live burst; `HttpState::health()` is the sole production caller, feeding it
/// `NetHealth::send_pressure_streak` and the age of `NetHealth::last_send_pressure_at` measured at
/// HTTP READ time (never cached — #343's rule for every age in this payload).
///
/// This is the previously-missing ALERT #656 asked for. `send_wouldblock_rescued`/`send_deferred`
/// are cumulative since process start and can only grow, so before this nothing could tell "was
/// starved once, an hour ago" from "is starved right now" — an agent polling `/v1/observe/debug`
/// had no way to know whether a climbing `send_deferred` meant anything was currently wrong.
///
/// Fires only once BOTH hold:
///   - the current burst has reached [`SEND_PRESSURE_FIRE_THRESHOLD`] consecutive events (filters
///     out the single-stray-`WouldBlock`-after-connect case, which is documented normal behavior,
///     not CPU pressure); AND
///   - the most recent pressure event is still within [`SEND_PRESSURE_BURST_GAP_SECS`] — so the
///     alert CLEARS on its own once the burst ends, rather than latching forever. This is exactly
///     what #656 asked for: a RATE/recency signal derived from the lifetime counters, not another
///     lifetime counter that only ever grows.
///
/// `last_pressure_ago` is `None` before this process has ever recorded a pressure event — correctly
/// reads as "not starved" (there is nothing to report yet, which is the honest healthy default).
pub fn send_starved(streak: u64, last_pressure_ago: Option<std::time::Duration>) -> bool {
    match last_pressure_ago {
        None => false,
        Some(ago) => {
            streak >= SEND_PRESSURE_FIRE_THRESHOLD
                && ago <= std::time::Duration::from_secs(SEND_PRESSURE_BURST_GAP_SECS)
        }
    }
}

pub type NetHealthShared = std::sync::Arc<std::sync::Mutex<NetHealth>>;

/// Smoothed per-frame phase timings, published by the **render** thread — see `PlayerState`'s note
/// on the network/render split. As of eqoxide#797, [`SkinCapDowngradesShared`] below is published
/// the same way, by the same thread, so this is no longer the only render-owned agent-visible value
/// (an earlier version of this doc claimed it was; that claim is what eqoxide#797 made false).
pub type FrameProfileShared = std::sync::Arc<std::sync::Mutex<FrameProfile>>;

/// One reported character-model joint-cap downgrade, as served to an agent over HTTP. The plain,
/// serializable HTTP-facing shape of `eqoxide_renderer::renderer::SkinCapDowngrade` — that type
/// cannot be used directly here, because `eqoxide-ipc` sits BELOW `eqoxide-renderer` in the crate
/// graph (see this crate's own layering note at the top of this file) and cannot name a renderer
/// type. `src/app.rs` converts one into the other once per frame, the same shape
/// `FrameProfile`/`FrameProfileShared` already established for a render-owned value crossing this
/// same layering boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct SkinCapDowngradeView {
    /// The joint count that exceeded the cap and caused the downgrade.
    pub joint_count: usize,
    /// True iff this report key has ever been written by two different source files — see
    /// `eqoxide_renderer::renderer::SkinCapDowngrade`'s doc (eqoxide#848) for why the key (a file's
    /// base name) can collide across two asset roots, and why this flag exists to disclose it
    /// rather than silently overwrite one rig's downgrade with another's.
    pub key_collision: bool,
}

/// Character models downgraded to the static (unskinned) render arm because their skin exceeded
/// `eqoxide_renderer::renderer::JOINT_CAP`, keyed by the loaded GLB's file name — the same map as
/// `eqoxide_renderer::renderer::EqRenderer::skin_cap_downgrades`, published for
/// `/v1/observe/debug`'s `skin_cap_downgrades` field (eqoxide#797; documented in
/// `docs/http-api.md`). Absent = no downgrade has happened; a missing or genuinely unskinned model
/// is never a key here.
///
/// Before eqoxide#797 this map existed but was reachable only from Rust code inside this process —
/// a driving agent with no other channel to the world had no way to read it, no matter how public
/// the renderer's own field was. That gap is exactly what this type and its publish path close.
pub type SkinCapDowngradesShared =
    std::sync::Arc<std::sync::Mutex<std::collections::BTreeMap<String, SkinCapDowngradeView>>>;

/// Aggro-avoidance knobs the `/v1/move/*` handlers set and the nav walker reads (#242). `enabled`
/// gates the always-on NPC-camp avoidance (#67) — `false` routes straight through (e.g. to reach a
/// mob). `buffer` widens the soft-avoid radius so the route gives hostile pulls more berth. Default =
/// the historical behavior (avoidance on, no extra buffer). A `/goto`/`/zone_cross` request that omits
/// the fields leaves the current setting unchanged.
#[derive(Clone, Copy)]
pub struct AggroAvoidOpts {
    pub enabled: bool,
    pub buffer:  f32,
}
impl Default for AggroAvoidOpts {
    fn default() -> Self { Self { enabled: true, buffer: 0.0 } }
}
pub type NavAvoidShared = Arc<Mutex<AggroAvoidOpts>>;

// (#608: the old `NavPathView` pair — the walker's committed coarse/fine plan for the 2D overlay —
// is GONE. The walker now publishes the full `eqoxide_nav::diagnostics::NavDebugSnapshot` (which
// carries the committed routes, the plan's per-edge trace, pad knowledge and more) through
// `eqoxide_nav::diagnostics::NavDebugView`. That slot cannot live in this crate: it names nav
// types, and `eqoxide-ipc` sits BELOW `eqoxide-nav` in the crate graph — so it is defined in
// `eqoxide-nav` and wired alongside `ControllerSlots` in `main.rs`. One published source; a second
// copy of the committed route here would be a drift channel.)

/// A name-keyed roster map that **cannot be mutated from outside this crate** (#643 review r3).
///
/// It derefs to `HashMap<String, V>`, so every existing read — `get`, `len`, `iter`, `keys`,
/// `contains_key`, `&*guard` into a `&HashMap` — works exactly as before, with no call-site
/// changes. What it deliberately does NOT implement is `DerefMut`, and it exposes no public
/// mutators. The only way to write one is [`WorldSlots::publish_entities`], which writes all three
/// roster maps together.
///
/// # Why a newtype instead of a plain `HashMap`
///
/// `/v1/observe/entities?labeled=1` promises an agent that `poses` is keyed exactly like
/// `entities`, so `body["poses"][name]` cannot `KeyError`. That promise is only as strong as its
/// weakest writer, and this repo has already broken it once: two publishers existed
/// (`eqoxide_net::action_loop::sync_entities` and `eqoxide_net::login`'s zone-in seed), and when
/// `entity_poses` was added only the first was updated.
///
/// Two weaker fixes were tried first and both were falsified by a reviewer:
///
/// 1. *Add the missing lines to the second loop.* The reviewer deleted them again; the whole
///    workspace suite stayed green. The invariant was a convention duplicated across two
///    hand-written loops.
/// 2. *Add a source scanner asserting there is only one publisher.* The reviewer wrote a third
///    publisher in the most idiomatic Rust form — `world.entity_positions.lock().unwrap()
///    .insert(..)`, mutation through a temporary guard with no binding at all — and the suite
///    stayed green, because the scanner keyed on `let mut` on the same line. It was pinned against
///    the bug that had already happened rather than the one that would happen next, which is the
///    same shape as the original defect. (That scanner had a second, independent hole too: its
///    "skip test modules" logic latched at the first `#[cfg(test)]` and never reset, so most of
///    several large production files went unscanned.)
///
/// So the rule moved into the type system, where a grep does not belong: a third publisher is now
/// a **compile error**, not a test failure and not a review catch. This is the same
/// make-the-bad-state-unrepresentable move `Pose`/`Gait` make one layer down.
///
/// # Test seeding
///
/// [`Roster::insert_for_test`] is gated on `#[cfg(any(test, feature = "test-fixtures"))]` so unit
/// tests can still seed a partial or deliberately-mismatched roster (several existing tests rely on
/// that, e.g. an ids-only fixture). Downstream crates enable `eqoxide-ipc/test-fixtures` as a
/// **dev-dependency** feature, so it is absent from `cargo build --release` entirely. It is named
/// `insert_for_test` rather than `insert` on purpose: if that feature ever did get enabled for a
/// normal dependency, a production call site would still read `insert_for_test` and be obvious in
/// review, instead of silently looking like ordinary map access.
///
/// # The exact strength of this guarantee (measured, not asserted)
///
/// - A production publisher written the idiomatic way — `world.entity_positions.lock().unwrap()
///   .insert(..)` — fails to compile under **both** `cargo test --workspace` and
///   `cargo build --release`, because [`Roster::insert`] is `pub(crate)` to this crate. That is the
///   shape a reviewer used to defeat the previous revision's source scanner.
/// - The same code written with `insert_for_test` fails `cargo build --release` (dev-dependency
///   features are absent there) but *does* compile under `cargo test --workspace`, where Cargo
///   unifies the workspace's dev-dependency features. CI runs `cargo build --release --locked`
///   BEFORE the test job, so it is still caught — but it is caught at build time, not by a test.
///   **That containment rests on CI's shape, and nothing durable pins it:** the build and test steps
///   are steps of the SAME job, steps run sequentially, and a failing step fails the job — so today
///   the release build genuinely gates the tests. Split them into separate jobs, reorder them, or
///   add `continue-on-error`, and this hatch silently reopens with no test noticing. If you touch
///   `.github/workflows/test.yml`, keep `cargo build --release` ahead of `cargo test` in one job.
///
/// That residual is deliberate and bounded: closing it entirely would mean giving up in-crate test
/// fixtures that seed partial rosters on purpose. It is recorded here so nobody has to rediscover
/// it, and so "a third publisher cannot compile" is read with its one qualification attached.
/// `Debug`/`PartialEq` only. **Every other derive or impl on this type is deliberately absent** —
/// see "The sealed surface" in the doc comment above.
#[derive(Debug, PartialEq)]
pub struct Roster<V>(HashMap<String, V>);

impl<V> std::ops::Deref for Roster<V> {
    type Target = HashMap<String, V>;
    fn deref(&self) -> &Self::Target { &self.0 }
}

// ── The seal ─────────────────────────────────────────────────────────────────────────────────
// NOT implemented, each one deliberately, each one a way to write a roster from outside this crate:
//
//   DerefMut     — would re-expose every `HashMap` mutator (`insert`, `remove`, `clear`, `entry`,
//                  `get_mut`, …) through the `MutexGuard`.
//   Default      — `*guard = Roster::default()` wipes a map. Replaced by `pub(crate) fn new()`.
//   FromIterator — `*guard = pairs.into_iter().collect()` REPLACES the whole map. This is the one
//                  that defeated the first version of this seal: it blocked per-entry mutation but
//                  left whole-value assignment open, and a complete third publisher written that
//                  way compiled clean in release. (The `DerefMut` in play there is `MutexGuard`'s,
//                  not `Roster`'s, so the missing impl below was simply routed around.)
//   Clone        — `*guard = kept_earlier.clone()` restores a stale roster into one map and not the
//                  others. Found while enumerating this list rather than by a failing build; with
//                  `Clone` gone, `.clone()` on a `Roster` resolves through `Deref` to
//                  `HashMap::clone`, which yields a `HashMap` that cannot be assigned back.
//   serde        — no `Deserialize`, so no deserialize-into-place either.
//
// What that list does and does not establish. It establishes exactly one thing: an outside crate
// cannot CONSTRUCT a `Roster<V>`. So every write that needs a freshly-built value is closed —
// `*guard = pairs.collect()`, `*guard = Default::default()`, `*guard = kept.clone()`,
// `mem::take(&mut *guard)`, `mem::replace(&mut *guard, ..)` — because each of them has to name a
// producer, and there is none.
//
// It does NOT establish that no `Roster` value can be named or moved, and an earlier revision of
// this comment claimed that it did. That was false. When `WorldSlots`' fields were `pub`,
// `publish_entities` was `pub`, and `MutexGuard` supplied `DerefMut`, an outside crate could
// legitimately populate a SCRATCH `WorldSlots` and then `mem::swap` one of its maps into a live one,
// MOVING an existing `Roster` without ever constructing one. That compiled clean in release (#665)
// and desynced `entity_ids` from `entity_positions`, which `combat.rs`'s "is this spawn known?"
// answers from alone. Closing the producer set was necessary but not sufficient: the remaining leak
// was that the CONTAINER handed out mutable access to what it protects.
//
// #665 closed that at the container: `WorldSlots`' three roster fields are now PRIVATE and its only
// read path is [`WorldSlots::entity_positions`] / `entity_ids` / `entity_poses`, which return a
// [`RosterReadGuard`] — a guard with NO `DerefMut`. With no `&mut Roster` reachable from outside the
// crate, there are no two `&mut Roster`s to `mem::swap`, so the move above is now a COMPILE error
// (proved by the `compile_fail` doctests on those accessors). Writes still go only through
// `publish_entities`, still the single publisher.
//
// The reason to close CONSTRUCTORS rather than call-site shapes is that producers are finite and
// enumerable, so "each member is closed" is a claim that can be checked. Two earlier attempts here
// guessed at shapes instead (`let mut` in a source scanner; then per-entry mutation) and each
// survived only the shapes someone happened to try.
// ─────────────────────────────────────────────────────────────────────────────────────────────

impl<V> Roster<V> {
    /// The only constructor. `pub(crate)` — hand-written rather than `Default` so it does not
    /// appear in this type's public API, and so it needs no `V: Default` bound (`EntityPoseView`
    /// has no meaningful default; there is no such thing as a default body pose).
    pub(crate) fn new() -> Self { Roster(HashMap::new()) }

    /// Drop every entry. `pub(crate)` — only the single publisher may write a roster.
    pub(crate) fn clear(&mut self) { self.0.clear(); }
    /// Insert one entry. `pub(crate)` — only the single publisher may write a roster.
    pub(crate) fn insert(&mut self, k: String, v: V) -> Option<V> { self.0.insert(k, v) }

    /// **Test fixtures only.** Seed one entry directly, bypassing the all-three-maps guarantee —
    /// which is exactly what a test wants when it needs a partial or intentionally-mismatched
    /// roster. Never available in a release build; see the type's doc comment.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn insert_for_test(&mut self, k: String, v: V) -> Option<V> { self.0.insert(k, v) }
}

/// A **read-only** lock guard over a [`Roster`], returned by [`WorldSlots`]'s roster accessors
/// (`entity_positions` / `entity_ids` / `entity_poses`).
///
/// It `Deref`s to `Roster<V>` (which in turn `Deref`s to `HashMap`), so every read the old public
/// `Arc<Mutex<Roster<..>>>` fields supported — `get`, `len`, `iter`, `keys`, `values`,
/// `contains_key`, `&*guard`, deref-coercion to `&HashMap` at a call boundary — works unchanged.
/// The callers that used to write `world.entity_positions.lock().unwrap()` now write
/// `world.entity_positions()`; nothing else about a read site changes.
///
/// It deliberately does **not** implement `DerefMut`. That is the whole point (#665).
///
/// # Why not just return the `MutexGuard`
///
/// A `MutexGuard<'_, Roster<V>>` supplies `DerefMut`, i.e. a `&mut Roster`. #652 sealed `Roster`'s
/// *constructors*, which closed every write that needs a freshly-built value — but two `&mut Roster`s
/// are all `std::mem::swap` needs to *move* one existing roster in place of another (from a populated
/// scratch `WorldSlots`), desyncing `entity_ids` from `entity_positions` without ever constructing a
/// `Roster`. Handing back a guard with no `DerefMut` removes the `&mut Roster` entirely, so there is
/// nothing to swap. See the compile-fail proofs on [`WorldSlots::entity_positions`].
pub struct RosterReadGuard<'a, V>(std::sync::MutexGuard<'a, Roster<V>>);

impl<V> std::ops::Deref for RosterReadGuard<'_, V> {
    type Target = Roster<V>;
    fn deref(&self) -> &Self::Target { &self.0 }
}
// No `DerefMut`, deliberately — a `&mut Roster` reachable from outside this crate is exactly the
// #665 leak (two of them let `mem::swap` move one map past the single-publisher rule).

/// Live entity name → (x, y, z) map, published by `WorldSlots::publish_entities`.
pub type EntityPositions = Arc<Mutex<Roster<(f32, f32, f32)>>>;

/// Live entity name → spawn_id map (same keys as EntityPositions).
pub type EntityIds = Arc<Mutex<Roster<u32>>>;

/// One entity's server-published body state, as exposed by `/v1/observe/entities?labeled=1` (#643).
///
/// Both halves are the wire's own signals, kept in their OWN domains — before #643 they shared a
/// single `u32` on `Entity`, so whichever packet arrived last silently decided what the number
/// meant. `pose` is the discrete body state; `gait` is the locomotion speed code.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct EntityPoseView {
    /// `standing` / `freeze` / `looting` / `sitting` / `crouching` / `lying`, or
    /// **`unknown(<raw>)`** when the server sent a code this client does not recognise. An
    /// unrecognised code is reported verbatim rather than guessed at (agent-honesty).
    pub pose: String,
    /// The most recent `OP_ClientUpdate` gait (locomotion speed) code, or `null` when this entity
    /// has not sent a position update yet. `null` means **"not reported"**, NOT "standing still".
    pub gait: Option<i32>,
}

/// Live entity name → pose/gait map (same keys as `EntityPositions`), published each tick by the
/// net thread and read by `GET /v1/observe/entities?labeled=1` (#643).
pub type EntityPoses = Arc<Mutex<Roster<EntityPoseView>>>;

/// Zone exit points received in OP_SEND_ZONE_POINTS, exposed via GET /v1/observe/zone_points.
pub type ZonePoints = Arc<Mutex<Vec<eqoxide_core::game_state::ZonePoint>>>;
/// Outcome of the most recent attempt to load the current zone's map `.txt` pack for the
/// client-synthesized `"to "`-label fallback entries `ActionLoop::sync_zone_points` merges into
/// [`ZonePoints`] (#816). `None` means the last attempt succeeded (or none has run yet this
/// process — same "null while healthy" convention as `HttpState::net_thread_dead` /
/// `common_assets_failed`). `Some(e)` means the fallback entries that zone's map WOULD have
/// contributed are UNKNOWN, not confirmed absent — see [`eqoxide_core::zone_map::ZoneMapLoadError`].
/// Published only by `sync_zone_points`'s zone-change branch; read by `GET /v1/observe/debug` as
/// `zone_map_load`.
pub type ZoneMapLoadShared = Arc<Mutex<Option<eqoxide_core::zone_map::ZoneMapLoadError>>>;
/// Native Task-system quest log, published from GameState.tasks each tick (GET /v1/observe/quests/log).
pub type TaskLog = Arc<Mutex<Vec<eqoxide_core::game_state::ActiveTask>>>;

/// Pending offers from an open task-selector window, published each tick (GET /v1/quests/offers).
pub type TaskOffersShared = Arc<Mutex<Vec<eqoxide_core::game_state::TaskOffer>>>;
/// Completed-task history with titles, published each tick (GET /v1/quests/completed).
pub type CompletedTasksShared = Arc<Mutex<Vec<eqoxide_core::game_state::CompletedTaskEntry>>>;
/// Accept/decline a pending task offer, set by POST /v1/quests/accept ({"task_id":N}) or
/// POST /v1/quests/decline (task_id=0). The nav thread reads it once and sends
/// OP_AcceptNewTask (AcceptNewTask_Struct), looking up the offering NPC's id from gs.task_offers.
pub type AcceptTaskReq = Arc<Mutex<Option<u32>>>;
/// Abandon an active task, set by POST /v1/quests/cancel ({"task_id":N}). The nav thread reads it
/// once, looks up the task's sequence_number in gs.tasks, and sends OP_CancelTask
/// (CancelTask_Struct).
pub type CancelTaskReq = Arc<Mutex<Option<u32>>>;

/// Read a book/note item, set by POST /v1/interact/read ({"slot":N}). Carries the inventory wire
/// slot of the item to read. The nav thread takes it, looks up the item's Filename in gs.inventory,
/// and sends OP_ReadBook; the server replies with the text (surfaced via /v1/observe/item_text). (#288)
pub type ReadBookReq = Arc<Mutex<Option<i32>>>;

/// One group member's live view for GET /v1/group/roster (role badges are read-only display
/// flags pushed by the server — not settable via this API in v1).
#[derive(Clone, serde::Serialize)]
pub struct GroupMemberView {
    pub name:     String,
    pub level:    u32,
    pub is_leader: bool,
    pub is_merc:  bool,
    pub tank:     bool,
    pub assist:   bool,
    pub puller:   bool,
    pub offline:  bool,
    pub hp_pct:   f32,
}

/// Published each nav tick from GameState.group_members/group_leader/pending_invite (GET
/// /v1/group/roster, and the UI roster panel). `you_are_leader` is precomputed at publish time
/// (gs.player_name == gs.group_leader) so handlers don't need the player's own name separately.
#[derive(Clone, Default)]
pub struct GroupSnapshot {
    pub members:         Vec<GroupMemberView>,
    pub leader:           String,
    pub pending_invite:   Option<String>,
    pub you_are_leader:   bool,
}
pub type GroupShared = Arc<Mutex<GroupSnapshot>>;

/// Published each nav tick from the player's guild identity + roster: the guild fields of
/// /v1/observe/debug and GET /v1/guild/roster. `guild_id == 0` / empty `guild_name` = not in a
/// guild. Mirrors GroupSnapshot. (#295)
#[derive(Clone, Default)]
pub struct GuildSnapshot {
    pub guild_id:   u32,
    pub guild_name: String,
    pub guild_rank: u32,
    pub members:    Vec<eqoxide_core::game_state::GuildMember>,
    /// Name of whoever has a pending guild invite out to us (for GET /v1/guild/roster), or None.
    pub pending_invite: Option<String>,
}
pub type GuildShared = Arc<Mutex<GuildSnapshot>>;

/// One queued guild action from POST /v1/guild/{invite,accept,leave,remove}, drained by the nav tick
/// which sends the matching RoF2 guild opcode. Bundled into one slot to keep the ActionLoop plumbing
/// small. (#295)
#[derive(Clone, Debug, PartialEq)]
pub enum GuildAction {
    Invite(String),   // POST /v1/guild/invite {"name"} — invite a player to our guild
    Accept,           // POST /v1/guild/accept — accept a pending guild invite
    Leave,            // POST /v1/guild/leave — leave our guild
    Remove(String),   // POST /v1/guild/remove {"name"} — leader/GM removes a member
}
pub type GuildActionReq = Arc<Mutex<Option<GuildAction>>>;

/// POST /v1/group/invite target name. Drained by the nav tick loop, which sends OP_GroupInvite.
pub type GroupInviteReq = Arc<Mutex<Option<String>>>;
/// POST /v1/trainer/open sets this to the trainer NPC's spawn id → nav sends OP_GMTraining (#99).
pub type TrainerOpenReq = Arc<Mutex<Option<u32>>>;
/// POST /v1/trainer/train sets this to a skill id → nav sends OP_GMTrainSkill for the open trainer.
pub type TrainerTrainReq = Arc<Mutex<Option<u32>>>;
/// POST /v1/group/accept trigger — accepts gs.pending_invite. One-shot: `Some(())` then drained.
pub type GroupAcceptReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/decline trigger — declines gs.pending_invite via a defensive OP_GroupDisband.
pub type GroupDeclineReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/leave trigger — sends OP_GroupDisband(self, self).
pub type GroupLeaveReq = Arc<Mutex<Option<()>>>;
/// POST /v1/group/kick target member name. Sends OP_GroupDisband(self, target).
pub type GroupKickReq = Arc<Mutex<Option<String>>>;
/// POST /v1/group/makeleader target member name. Sends OP_GroupMakeLeader.
pub type GroupMakeLeaderReq = Arc<Mutex<Option<String>>>;

/// Zone-crossing request set by POST /v1/move/zone_cross; gameplay thread reads it once,
/// teleports to the matching zone line and sends OP_ZONE_CHANGE.
///   Some(0)  → cross the nearest zone line (any destination).
///   Some(id) → cross to a specific destination zone id.
pub type ZoneCrossReq = Arc<Mutex<Option<u16>>>;

/// Manual-movement escape hatch (#188), set by POST /v1/move/manual or /v1/move/jump. The render
/// loop drives the CharacterController with this — exactly like WASD — taking priority over the
/// `/goto` nav planner (but below real keyboard input) until `until`, so an agent can walk/hop out
/// of a spot where A* finds no path. `dir` is a world `(east, north)` direction (zero = stand in
/// place, e.g. a jump with no movement). `up` is the vertical axis for swimming (`-1..1`): while in
/// water it drives the character up/down through the water column (#207); it's ignored on land.
#[derive(Clone, Copy)]
pub struct ManualMove {
    pub dir:   [f32; 2],
    pub up:    f32,
    pub jump:  bool,
    pub until: std::time::Instant,
}
pub type ManualMoveReq = Arc<Mutex<Option<ManualMove>>>;

/// A hail request set by POST /v1/interact/hail: the NPC's display name (for the "Hail, <name>"
/// say text) plus its `spawn_id` when known. The nav thread targets the NPC (`spawn_id`) BEFORE
/// saying, because the server only fires an NPC's `EVENT_SAY` on the player's current target
/// (client.cpp: `Mob* t = GetTarget()`), so a hail without a target is silently ignored (#130).
pub type HailReq = Arc<Mutex<Option<(String, Option<u32>)>>>;

/// Arbitrary Say-channel text, set by POST /v1/interact/say or a HUD button/keyword; the nav thread
/// reads it once and sends it on the Say channel (used for quest keyword follow-ups).
pub type SayReq = Arc<Mutex<Option<String>>>;

/// Spawn id to target, set by POST /v1/combat/target or the HUD "Target nearest" button; the nav
/// thread reads it once, sends OP_TargetCommand + OP_Consider.
pub type TargetReq = Arc<Mutex<Option<u32>>>;

/// Auto-attack toggle — set to true by POST /v1/combat/attack, false by DELETE /v1/combat/attack.
/// Nav thread reads it and sends OP_AUTO_ATTACK(1) or OP_AUTO_ATTACK(0).
pub type AttackReq = Arc<Mutex<Option<bool>>>;

/// Buy request — (merchant spawn id, merchant inventory slot), set by POST /v1/merchant/buy.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerBuy (buy that slot).
/// This is the FIRE-AND-FORGET buy the UI merchant-window click uses; the honest awaited variant
/// (POST /v1/merchant/buy over HTTP) rides the sibling [`BuyAwaitReq`] instead. (#448)
pub type BuyReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Command-with-result buy request (A3 Migration 1, #448) — `(merchant spawn id, merchant slot,
/// oneshot Sender)`. POST /v1/merchant/buy writes this and AWAITS the `Sender`; the nav thread
/// drains it, sends the same OP_ShopRequest + OP_ShopPlayerBuy the fire-and-forget [`BuyReq`] path
/// sends, and PARKS the `Sender` in `ActionLoop::pending_buy` until the resolving packet
/// (OP_ShopPlayerBuy echo → `Resolved`, OP_ShopEndConfirm → `Refused`) is applied — or the HTTP
/// timeout / a reaper yields `Unconfirmed`. Sibling of [`BuyReq`], NOT a replacement: the two slots
/// coexist so the UI click path is unchanged. See [`result`] for the flow.
pub type BuyAwaitReq = Arc<Mutex<Option<(u32, u32,
    oneshot::Sender<CommandResult<BuyOk>>)>>>;

/// Sell request — (merchant spawn id, player inventory slot, quantity), set by POST /v1/merchant/sell.
/// Nav thread reads it and sends OP_ShopRequest (open) + OP_ShopPlayerSell (sell that slot).
pub type SellReq = Arc<Mutex<Option<(u32, u32, u32)>>>;

/// Manual pet command — one OP_PetCommands command byte (PET_ATTACK=2, PET_FOLLOWME=4,
/// PET_GUARDHERE=5, PET_SIT=6, PET_BACKOFF=28; EQEmu zone/common.h), set by POST /v1/pet/command
/// or a Pet-window button. The nav thread drains it and sends OP_PetCommands (attack uses the
/// current target as PetCommand_Struct.target; other commands send target 0).
pub type PetCmdReq = Arc<Mutex<Option<u8>>>;

/// Open/close a merchant window. `Open(merchant_id)` from POST /v1/merchant/open; `Close` from
/// POST /v1/merchant/close. The nav thread sends OP_ShopRequest (command 1/0).
/// This is the FIRE-AND-FORGET open/close the UI merchant-window click uses; the honest awaited
/// open (POST /v1/merchant/open over HTTP) rides the sibling [`OpenAwaitReq`] instead. (#479)
#[derive(Clone, Copy)]
pub enum TradeCmd { Open(u32), Close }
pub type TradeReq = Arc<Mutex<Option<TradeCmd>>>;

/// Command-with-result merchant-open request (A3 migration, eqoxide#479) — `(merchant spawn id,
/// oneshot Sender)`. POST /v1/merchant/open writes this and AWAITS the `Sender`; the nav thread's
/// `drain_merchant` drains it, sends the SAME OP_ShopRequest(command=1) the fire-and-forget
/// [`TradeReq`] `Open` path sends, and PARKS the `Sender` in `ActionLoop::pending_open` until the
/// resolving OP_ShopRequest echo lands: `command==1` → `Resolved(OpenOk)` (a real merchant opened
/// the window); `command==0` → `Refused` (a REAL negative ack — RoF2's Handle_OP_ShopRequest
/// collapses faction-KOS/engaged/feigned-invis/charmed/already-busy into this same echo). A target
/// that is not a merchant at all, or out of range, sends NO echo whatsoever (confirmed against the
/// EQEmu RoF2 source — see `~/git/eq_kb/merchant-open-protocol.md`) — that path
/// resolves to `Unconfirmed` via the HTTP timeout / a zone-change reaper, never a fabricated 200.
/// Sibling of [`TradeReq`], NOT a replacement: the UI open/close click path is unchanged. See
/// [`result`] for the flow.
pub type OpenAwaitReq = Arc<Mutex<Option<(u32,
    oneshot::Sender<CommandResult<OpenOk>>)>>>;

/// Camp command, written by POST /v1/lifecycle/exit, POST /v1/lifecycle/camp, the HUD Camp button,
/// and the `/camp` chat keyword. The gameplay loop drains it: `Start` begins a camp if one isn't
/// running (idempotent — used by /exit so a double request doesn't cancel); `Toggle` starts a camp
/// or cancels the one in progress (used by the button / chat command). A completed camp shuts the
/// client down cleanly (no linkdead) once the server's ~29s camp timer has elapsed. See
/// `gameplay::camp_apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CampCmd { Start, Toggle }
pub type CampReq = Arc<Mutex<Option<CampCmd>>>;
/// Set true by POST /v1/lifecycle/respawn to release a held-dead character back to its bind point
/// (the client no longer auto-respawns — it holds the character slain until asked). (#284)
pub type RespawnReq = Arc<Mutex<bool>>;

/// Published camp state: `Some(deadline)` while a camp is in progress (the instant the client will
/// disconnect), `None` otherwise. Set by the gameplay loop; read by the HUD for the countdown and
/// by handlers to know whether a camp is already running.
pub type CampUntil = Arc<Mutex<Option<std::time::Instant>>>;

/// Live merchant-session snapshot published each nav tick, read by GET /v1/merchant/list and used
/// for the HUD merchant window. `open` mirrors `GameState::merchant_open`.
#[derive(Default, Clone, serde::Serialize)]
pub struct MerchantSnapshot {
    pub open: bool,
    pub merchant_id: Option<u32>,
    pub items: Vec<eqoxide_core::game_state::MerchantItem>,
}
pub type MerchantShared = Arc<Mutex<MerchantSnapshot>>;

/// Move-item request — (from_slot, to_slot), set by POST /v1/inventory/move.
/// Nav thread reads it and sends OP_MoveItem (MoveItem_Struct, number_in_stack=1).
/// Used to equip/unequip/rearrange items (e.g. boots in bag slot 23 -> worn slot 19).
pub type MoveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Give request — (npc_spawn_id, item_from_slot), set by POST /v1/interact/give.
/// Nav thread runs the trade-window turn-in: puts the item on the cursor, sends OP_TradeRequest,
/// waits for OP_TradeRequestAck, then moves the item into the NPC trade slot + OP_TradeAcceptClick.
/// This is the FIRE-AND-FORGET give the UI turn-in path uses; the honest awaited variant (POST
/// /v1/interact/give over HTTP) rides the sibling [`GiveAwaitReq`] instead. (#448)
pub type GiveReq = Arc<Mutex<Option<(u32, u32)>>>;

/// Command-with-result give request (A3 Migration 2, #448) — `(npc spawn id, item from_slot,
/// oneshot Sender)`. POST /v1/interact/give writes this and AWAITS the `Sender`; the nav thread's
/// `tick_give` state machine drives the SAME trade-window turn-in the fire-and-forget [`GiveReq`]
/// path drives, and PARKS the `Sender` inside its `GiveState` until the resolving packet lands:
/// OP_FinishTrade (the NPC accepted the item) → `Resolved(GiveOk)`; the no-ack / no-finish abort →
/// `Unconfirmed`; a second awaited give while one is in flight → `Refused` (singleton-in-flight).
/// Sibling of [`GiveReq`], NOT a replacement — the two slots coexist so the UI turn-in path is
/// unchanged. See [`result`] for the flow.
pub type GiveAwaitReq = Arc<Mutex<Option<(u32, u32,
    oneshot::Sender<CommandResult<GiveOk>>)>>>;

/// Live snapshot of the player's inventory + equipment, published each tick by the nav thread
/// and read by GET /v1/observe/inventory. Slots are Titanium **wire** ids (the same numbers /give
/// and /inventory/move take — note these are one less than the EQEmu DB `inventory.slot_id` for
/// general slots: DB 23-30 → wire 22-29).
pub type InventoryShared = Arc<Mutex<Vec<eqoxide_core::game_state::InvItem>>>;

/// Loot request — a corpse spawn id, set by POST /v1/interact/loot. The nav thread reads it once and
/// pushes the corpse onto the auto-loot queue (OP_LootRequest → OP_LootItem echoes → OP_EndLootRequest).
pub type LootReq = Arc<Mutex<Option<u32>>>;

/// One machine-readable line from the in-game message log (GET /v1/observe/messages). `kind` is the
/// channel ("npc" = NPC dialogue/emotes, "chat", "combat", "system", "exp", "loot", "trade",
/// "zone", …); `keywords` are the `[bracketed]` quest reply words extracted from the text (say them
/// back via POST /v1/interact/say to advance dialogue quests); `item_links` are any EQ item/say
/// links the text contained — `text` already shows only the clean display name (the raw hex link
/// body is never sent to an agent), and `item_links` gives the resolvable `item_id` behind each one
/// (eqoxide#256). Empty when the line had no links.
#[derive(Clone, serde::Serialize)]
pub struct MessageEntry {
    pub kind:        String,
    pub text:        String,
    pub keywords:    Vec<String>,
    pub item_links:  Vec<eqoxide_core::game_state::ItemLink>,
}

/// Live snapshot of the in-game message log, published each tick by the nav thread and read by
/// GET /v1/observe/messages. Exposes NPC dialogue (kind "npc") as machine-readable text + keywords.
pub type MessagesShared = Arc<Mutex<Vec<MessageEntry>>>;

/// Live snapshot of the current clickable NPC-dialogue choices (saylinks from the most recent NPC
/// message), published each tick by the nav thread and read by GET /v1/observe/dialogue. (#120)
pub type DialogueShared = Arc<Mutex<Vec<eqoxide_core::game_state::DialogueChoice>>>;

/// Live navigation state for the active `/move/goto`, set by the nav thread and read by
/// GET /v1/observe/debug. `state` is the agent-facing contract documented in `docs/http-api.md`:
///
/// `pending` | `idle` | `planning` | `navigating` | `navigating_partial` | `navigating_stalled` |
/// `following` | `arrived` | `no_path` | `search_exhausted` | `blocked` | `zone_loading`
///
/// The three `navigating*` words are not written as literals by the walker: they are DERIVED, every
/// drive tick, from a typed verdict by `nav::steering::driving_nav_state` (#851), so the walker
/// cannot publish a progress word while its own two-channel progress signal says the body is going
/// nowhere. Before that, the entire ~32 s stall/back-off/re-path recovery window published a bare
/// `navigating`.
///
/// `reason` is the machine-readable WHY behind a terminal state (`goal_not_walkable`,
/// `search_closed`, `search_node_cap`, …). The whole point of the split (#337): a driver must be
/// able to tell "there is no route" (definitive) from "the planner gave up" (I don't know) from
/// "I am wedged" — three answers the old, overloaded `blocked` collapsed into one silent freeze.
#[derive(Clone, Debug, PartialEq)]
pub struct NavStatus {
    pub state:  String,
    pub reason: Option<String>,
    /// GOAL IDENTITY (#349): a monotonically increasing generation stamp, bumped every time a NEW
    /// navigation request (`/move/{goto,follow,zone_cross,stop}`) is accepted. `state` is the status
    /// *of goal `goal_id`* — never of some earlier goal. Without this, a read right after a fresh
    /// `POST /goto` could return the PREVIOUS goto's terminal `arrived`/`no_path`/`blocked` (the
    /// walker only re-labels `state` on its next ~150ms tick), letting an agent conclude the new goto
    /// already finished. Each accept resets `state` to `pending` and bumps this ATOMICALLY (under the
    /// same lock), so goal N's terminal value can never be attributed to goal N+1. `0` = no request
    /// has been issued this session/zone. Surfaced as `nav_goal_id` on GET /v1/observe/debug; echoed
    /// in each accepting POST's response body.
    pub goal_id: u64,
    /// The goal coordinates `[x, y, z]` this `goal_id` is navigating to (server coords), so a caller
    /// can correlate "this state is for the goal I asked for". `None` for `idle`/`stop` (no goal) and
    /// for a `zone_cross` before the walker has resolved the concrete zone-line destination. Surfaced
    /// as `nav_goal` on GET /v1/observe/debug.
    pub goal: Option<[f32; 3]>,
    /// The agent-honesty payload behind a terminal `no_path` (#378 Phase 2): WHAT is blocking the
    /// goal, and WHERE. `blocked_goal` is the definitive "your goal itself cannot be stood at";
    /// `blocked_frontier` is "I got as close as here and this is the obstruction between me and the
    /// goal". Surfaced as `nav_blocked_by.goal` / `nav_blocked_by.frontier` on /v1/observe/debug.
    /// `None` when there is no blockage to report (not a terminal no_path, or the diagnosis could
    /// not be computed) — honest silence, never a fabricated hazard.
    pub blocked_goal: Option<NavBlockage>,
    pub blocked_frontier: Option<NavBlockage>,
    /// Which clearance tier answered the CURRENT route (#378 Phase 2 / design §4c): `preferred`
    /// (roomy) or `minimum` (threaded a tight gap with no margin to spare — a riskier path). `None`
    /// until a route is committed. This is the PER-ROUTE fact the zone-lifetime `nav_tight` counter
    /// could not give (it is `connected:true`'s shape — a field with no per-instance writer, #343).
    pub tier: Option<&'static str>,
    /// The FINE LOCAL (2 u) steering tier's last honest outcome (#382), published as the top-level
    /// `nav_local` on GET /v1/observe/debug. `None` = the tier has not answered for the current route
    /// (idle, or the first fine plan is still in flight).
    ///
    /// It is carried HERE, alongside `state`/`reason`, rather than in a second shared cell, because
    /// the two are read together and must not be able to drift: an agent that sees
    /// `nav_state: navigating` needs to know, in the same snapshot, whether the tier that is actually
    /// steering it can see a way through the next 40 u.
    pub local:  Option<NavLocal>,
    /// **The walker HAS a route and is not executing it (#851).** `Some` exactly while
    /// `state == "navigating_stalled"`, `None` otherwise — the two are written together, from one
    /// verdict, by `Walker::publish_drive_state`, so the word and its calibration data cannot drift.
    ///
    /// This field is the *evidence* behind the word, not a second opinion about it. An agent that
    /// only reads `nav_state` already gets the honest answer; this says how long, how hard the
    /// walker has already tried, and whether the route it is failing to execute even reaches the
    /// goal — which is what an agent needs to decide between waiting, re-issuing and giving up.
    ///
    /// Retired on every state change, for the same reason `tier` is: it is a fact about the route
    /// being executed now. That is a property of all three exhaustive writers, not of a habit —
    /// [`NavStatus::retire_to_idle`] (the goal ended), [`NavStatus::stamp_fresh_goal`] (a new goal
    /// replaced it) and [`NavStatus::transition_within_goal`] (the same journey moved state) each
    /// destructure this struct with no `..`, so this field could not have been added without a
    /// decision being recorded in all three. It was, in the first, when this field was introduced;
    /// the other two were flat lists then and one of them leaked a dead goal's payload under the
    /// next goal's `pending` (#851 review round 1, B1).
    pub stall:  Option<NavStall>,
    /// **The fine worker thread has died — latched, and scoped to that WORKER** (#766 review B3/B9).
    /// Set `true` the instant `LocalPlanner::is_dead()` is observed, and cleared by **nothing on any
    /// nav route**: no goal, no zone change, no retirement touches it, because the thread does not
    /// come back and recovering it needs a client restart. The one writer that clears it is
    /// `Walker::new` (`eqoxide-nav`), which does so as it spawns a REPLACEMENT worker — so what the
    /// latch describes is the worker, not the process. Those coincide today, exactly one `Walker`
    /// being built per process — a premise checked, to the limited extent a source scan can check it,
    /// by `walker::tests::exactly_one_production_fine_worker_is_built_in_the_tree_787` (#787) — which
    /// is why the agent-facing docs call the field session-scoped; the
    /// last paragraph here says why the distinction is worth keeping anyway. Published as the
    /// top-level `nav_local_planner_dead` on GET /v1/observe/debug, always, in both states: an agent
    /// checking its own health needs to be able to read "alive", not merely fail to read "dead".
    ///
    /// This field exists because `local` could not carry the fact honestly. `NavLocal` is a PER-GOAL
    /// verdict and #766 retires it with the goal, but `planner_dead` was riding in it as one of its
    /// three publishable `state` values while being a fact about the *client's fine worker*, not a
    /// statement about any goal — and `local` was its only publication surface in the tree (the
    /// `no_path`/`planner_dead` pair on `state`/`reason` comes from the COARSE planner). So
    /// retirement destroyed it, and the review found the consequence: an agent between goals — exactly
    /// when it polls `/v1/observe/debug` to decide what to do next — could not learn that its fine
    /// planner was dead. Splitting the worker fact out of the per-goal row fixes that without a
    /// carve-out in [`NavStatus::retire_to_idle`], which would have re-opened the very
    /// clear-`local`-on-every-`idle` uniformity #766 exists to create.
    ///
    /// **Known limit, stated rather than implied.** `LocalPlanner`'s death is only *discoverable*
    /// through a failed send or a disconnected receive, both of which happen on a tick that has a
    /// committed route. A worker that dies and is never posted to again is not detectable by any
    /// reader, this field included; what the latch guarantees is that once the death HAS been seen it
    /// stays visible for the rest of that worker's life — which, on today's one-`Walker` process, is
    /// the rest of the session — instead of dying with the next goal.
    ///
    /// **Latched for the life of a WORKER, and cleared where one is spawned** (rounds 3–5 review).
    /// "Session" means PROCESS today: `LocalPlanner::spawn` is reached only through `Walker::new` →
    /// `ActionLoop::new`, and the one production call site of `ActionLoop::new` is in
    /// `run_login_flow`, which returns as soon as the gameplay phase ends — so exactly one fine
    /// worker exists per process and "latched forever" and "latched for this worker" coincide.
    /// **That premise now has a TRIPWIRE under it, not a pin** (#787):
    /// `walker::tests::exactly_one_production_fine_worker_is_built_in_the_tree_787` in `eqoxide-nav`
    /// is a whole-tree source scan that fails — naming THIS sentence and the three others that rest
    /// on it — when a second *plainly-written* `Walker::new` or `LocalPlanner::spawn` construction
    /// site appears. Do not read it as stronger than that. It counts construction SITES; the premise
    /// is about construction EVENTS, and the two come apart on the in-process relogin this comment is
    /// about — one site, called twice, leaves the scan green. That was measured. Its rustdoc carries
    /// the full table of what it does and does not see. The B9 test
    /// below is NOT the pin either, because building a second `Walker` is its method. They
    /// stop coinciding the moment anything builds a second `Walker` over this row (the shape an
    /// in-process relogin would take): a NEW, healthy `LocalPlanner` would inherit `true` and the
    /// client would report a fault it had just repaired, permanently — #343's shape, and a lie in the
    /// honesty-critical direction. So `Walker::new` clears this flag as it spawns the worker, tying
    /// the latch's lifetime to the worker's rather than to the row's. That is a no-op on today's
    /// single-`Walker` process and is guarded by
    /// `a_new_walker_does_not_inherit_a_previous_workers_death_766` in `eqoxide-nav`, which
    /// constructs over a dirty row directly — the relogin *scenario* has no route to test through,
    /// but the *clear* does, and the clear is what carries the guarantee.
    ///
    /// **That covers the BIRTH end of a worker's life and nothing covers the death end** (#766 review
    /// B13). There is no `Drop` for `Walker` or for `LocalPlanner` anywhere, so when the net thread
    /// ends — `run_net_thread` in `src/model.rs` writes a terminal reason on all four of its exit
    /// arms — the worker is gone while this row, which the HTTP surface holds its own `Arc` to, goes
    /// on publishing whatever it last held, `false` included. So do NOT read this field as "the flag
    /// can never outlive the thread it reports on": a stale `false` after teardown is exactly that.
    /// It is *disclosed* rather than hidden — `net_thread_dead` is non-null on precisely those paths
    /// and the endpoint marks the whole payload a frozen final snapshot — which is why the review
    /// asked for the sentence to be corrected and explicitly did **not** ask for a teardown writer:
    /// adding one would be a new, untested route to fix something an existing signal already tells
    /// the agent.
    pub local_planner_dead: bool,
}

/// A named obstruction with a position — the agent-facing form of `traversability::Blockage`
/// (#378 Phase 2). `hazard` is `floor` | `wall` | `water`.
#[derive(Clone, Debug, PartialEq)]
pub struct NavBlockage {
    pub hazard: &'static str,
    pub at: [f32; 3],
}

/// The calibration data behind `nav_state: "navigating_stalled"` (#851) — a walker that has a route
/// and is not executing it.
///
/// It exists because the state word alone cannot answer "is this about to clear, or is it the
/// beginning of the 32 s wedge?". Published as the top-level `nav_stall` on GET /v1/observe/debug,
/// `null` whenever the walker is not stalled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NavStall {
    /// Consecutive nav ticks (~150 ms) since the walker last made progress by EITHER channel — the
    /// route cursor advancing by walking, or the closest 3-D approach to the goal improving. Equal
    /// to `nav::steering::NAV_STUCK_TICKS` on the first stalled tick and rising from there.
    pub quiet_ticks: u32,
    /// **The same window `quiet_ticks` counts, in milliseconds**: measured from the walker's last
    /// progressing tick, so on the first stalled tick it reads ≈ `NAV_STUCK_TICKS × 150` (~3000),
    /// not `0`.
    ///
    /// It is MEASURED, never derived from `quiet_ticks` × a nominal tick — the 150 ms nav tick is a
    /// floor, not a guarantee, so under load this runs longer than the arithmetic. That is the only
    /// reason the two disagree.
    ///
    /// It used to be measured from the tick the VERDICT flipped, which made it read `0` at the
    /// moment a stall was first announced: a uniform ~3 s understatement of how long the body had
    /// been going nowhere, sitting beside a `quiet_ticks` that counted the whole window (#851 review
    /// round 1, B2c). An understatement that makes an agent wait longer is still something the agent
    /// cannot detect from the payload, so it was corrected rather than documented.
    pub quiet_ms: u64,
    /// Stall-recovery re-paths run at this spot so far. The walker gives up at 8 with
    /// `blocked`/`walker_stalled`, so this is also "how much runway is left".
    pub repaths: u32,
    /// `complete` | `partial` — whether the route the walker is failing to execute even reaches the
    /// goal. A stalled PARTIAL is a weaker claim than a stalled complete route: the word
    /// `navigating_stalled` covers both, and this is how they are told apart.
    pub route: &'static str,
}

/// What the fine 2 u steering tier last said, verbatim. See `nav::collision::LocalOutcome`.
///
/// **`state` is never `no_path` and structurally cannot be.** The fine search closes only the frontier
/// inside a 40 u window, so it can never prove a goal unreachable; conflating its local dead-end with
/// a definitive "no route" would be #337 with a smaller radius.
#[derive(Clone, Debug, PartialEq)]
pub struct NavLocal {
    /// `threaded` (healthy: a complete fine route to the carrot) | `no_way_through` (the window's
    /// frontier CLOSED — the coarse corridor is not threadable here) | `exhausted` (the search was
    /// cut short: "I don't know") | `planner_dead` (the fine worker died; steering has degraded to
    /// the coarse route only — the walker keeps walking).
    pub state:       String,
    /// `threaded` | `search_closed` | `start_isolated` | `goal_not_walkable` | `no_geometry` |
    /// `search_node_cap` | `local_planner_dead`.
    pub reason:      String,
    /// Consecutive nav ticks the fine tier has failed to thread to its carrot. A nonzero value with
    /// `state: navigating` means the walker is being steered on the coarse route through a stretch the
    /// fine tier says it cannot fit — usually the prelude to a proactive coarse re-plan (#246).
    pub stuck_ticks: u32,
    /// How long the last fine plan took, in microseconds. This is the per-tick cost that used to be
    /// paid **on the network thread** (mean 15.3 ms, worst 358 ms, release/akanon) and is now paid on
    /// the fine worker.
    pub plan_us:     u64,
}

impl NavStatus {
    /// Retire to `idle`: the goal is over, so every fact that was ABOUT that goal goes with it.
    ///
    /// **Every production TRANSITION into `idle` goes through here**, which is what makes the
    /// `docs/http-api.md` state table's "`nav_goal` is `null` for `idle`/`stop`" a checked invariant
    /// rather than prose. The three writers that reach `idle` — `Walker::set_nav_state_because`
    /// (`zoned`, `goal_dropped`, `respawned`), `CommandState::stamp_new_goal` (`stopped`,
    /// `goto_superseded`) and `ZoneCrossTicket::drop` (`zone_cross_dropped_unhandled`) — all
    /// delegate here; that is all six documented `nav_reason` values for `idle`.
    ///
    /// The one `idle` that does NOT come through here is [`NavStatus::default`], the boot row. That
    /// is not a retirement: it builds the struct from scratch with every field at its zero value, so
    /// it cannot leave a previous goal's fact standing. It is also the only `idle` allowed to carry
    /// `reason: None` (#725 B1) — see the `debug_assert` below.
    ///
    /// Before #732 the row was documented but not enforced: `Walker::set_nav_state_because` retired
    /// `blocked_*`/`tier` on a transition and never touched `goal`, so every walker-side route to
    /// `idle` left the finished goal's `[x, y, z]` published beside it. Across a zone change that is
    /// the sharp case: coordinates are a per-zone namespace and carry no zone tag, so the pair reads
    /// as a well-formed answer about a zone the numbers do not belong to.
    ///
    /// Called unconditionally (never gated on "the state actually changed"): defence in depth, so no
    /// caller can reintroduce the leak by making a second retirement a no-op. (#732 review N1: under
    /// the fixed code every route clears `goal`, so an already-`idle` row has nothing left to clear —
    /// this guards the *shape*, not a scenario I can exhibit.)
    ///
    /// **The destructure is exhaustive with no `..` on purpose.** Adding a field to [`NavStatus`]
    /// is an `error[E0027]` here until someone decides whether it survives a goal's retirement —
    /// the same construction `AssetSyncState::slots()` uses in `eqoxide-ipc::asset_sync`. Note the
    /// weaker precondition: `NavStatus`'s fields are `pub` and read directly by several crates, so
    /// this pins the *retirement path* only. It does not stop other code writing the fields.
    ///
    /// **All THREE state-changing routes carry that net now, not just this one** (#851 review round
    /// 1, B1). Until then `idle` was the only exhaustive writer, and the other two were flat
    /// assignment lists — which is exactly how `stall` came to be decided here and forgotten there.
    /// Its siblings are [`NavStatus::stamp_fresh_goal`] (a new goal supersedes the old one) and
    /// [`NavStatus::transition_within_goal`] (the walker moves state inside one journey). A field
    /// added to [`NavStatus`] is now force-decided on every route out of a state, not one of three.
    ///
    /// **#766 moved `local` from KEPT to retired.** #732 left it standing with a `_keep_local`
    /// binding on the grounds that the FINE tier is "a different tier" whose clearing
    /// `Walker::clear_local_plan` owns. That reasoning holds for the tier's *machinery* and it still
    /// does — this function does not touch `LocalPlanner`, and every NON-`idle` state keeps `local`
    /// exactly as before, which is what preserves #382's deliberate keep-the-fine-verdict-on-`blocked`
    /// design (`Walker::stop_nav_blocked` publishes `blocked`/`no_path`, never `idle`, so it does not
    /// come through here). It does not hold for the published FIELD on an `idle` row: a `NavLocal`
    /// carrying `no_way_through` or `exhausted` is the fine planner's verdict on threading toward *the
    /// goal that just ended*, so it is a per-goal fact by the same argument as `tier`.
    ///
    /// **That argument covers two of the three publishable states, not all three** (review B3).
    /// `planner_dead` was never a verdict about a goal — it is a latched client fault, scoped to the
    /// fine WORKER rather than to any goal (round-6 review B12; the session framing this paragraph
    /// used to carry is the agent-facing one, and `local_planner_dead`'s own doc says why the two
    /// coincide today), that happened to be riding in this field as one of its three publishable
    /// `state` values, and retiring it with the
    /// goal would hide a dead fine worker from an agent between goals. It is not carved out here;
    /// it now has its own field, `local_planner_dead`, which this function KEEPS.
    ///
    /// Before #766 the routes did not agree with each other. `zoned` — the reported one — and
    /// `zone_cross_dropped_unhandled` left the verdict standing. The rest already cleared it, by two
    /// different mechanisms that are RUN here rather than read off the source: `goal_dropped` (and
    /// `respawned`, which shares its branch) because `Walker::resolve_goal`'s no-goto branch calls
    /// `clear_local_plan()` on the same tick before it retires — `eqoxide-nav`'s
    /// `the_goal_dropped_route_already_cleared_the_fine_verdict_before_766`; and the command-side
    /// ones through an explicit `s.local = None;` in `CommandState::stamp_new_goal`, now deleted as
    /// redundant — `eqoxide-command`'s
    /// `every_command_side_retirement_retires_the_fine_tiers_verdict_766`. Routing them all through
    /// here replaces that agreement-by-coincidence with one writer. (`respawned` is covered by
    /// reading the shared branch, not by its own test.)
    ///
    /// **This covers the transition only.** `docs/http-api.md` states `nav_local: null` as a
    /// universal over every `idle` row, and a retirement writer cannot deliver that on its own — the
    /// fine tier publishes from another thread and can land a verdict after the row went `idle`.
    /// The other half of the guarantee is the coercion in `Walker::set_nav_local`; see its doc
    /// comment for why that one is a coercion and not an assert.
    ///
    /// **The `stop_nav_blocked` half of the #382 argument is true by convention, not by
    /// construction.** Its `state` is a `&str`, so nothing stops a future caller passing `"idle"`
    /// and routing a terminal `blocked` through this retirement after all. Every call site in the
    /// tree today passes a literal — `blocked`, `no_path`, `search_exhausted` — so the design holds
    /// now, but it is grep-checkable, not enforced.
    ///
    /// The structural remedy is a typed `state` — an enum whose `idle` variant `stop_nav_blocked`
    /// cannot name — and that is a workspace-wide change, out of this issue's scope. A
    /// `debug_assert!(state != "idle", …)` would NOT be that remedy, and the earlier draft of this
    /// paragraph was wrong to call it structural (review B4): `debug_assert!` compiles out under
    /// `--release`, so it is a test-time instrument. That is not a new opinion: it is the same
    /// argument `Walker::set_nav_local`'s doc makes for taking a coercion instead of an assert, and
    /// the repo already says it out loud about the #725 writer guard — the doc on
    /// `a_reasonless_idle_is_refused_by_the_writer_not_just_by_a_per_call_site_test_725` in
    /// `eqoxide-nav` calls that `debug_assert!` "a TEST-TIME instrument, not a runtime one". It would
    /// raise
    /// the odds of catching a bad call site in CI; it would not make the invariant hold in the shipped
    /// binary. Recorded as a known limit rather than left implied.
    pub fn retire_to_idle(&mut self, why: Option<&str>) {
        // The same writer-level guard as `Walker::set_nav_state_because` and
        // `CommandState::stamp_new_goal`: on `idle`, `nav_reason: null` is reserved for boot (#725).
        debug_assert!(why.is_some(),
            "#725 B1: `idle` must name how it got there; `nav_reason: null` is reserved for boot");
        let NavStatus {
            state, reason,
            // KEPT, deliberately. `goal_id` is a monotonic IDENTITY stamp, not a per-goal fact: it
            // is what lets a caller say "this `idle` is the outcome of the goal I asked for" (#349).
            // Zeroing or bumping it here would break that correlation.
            goal_id: _keep_goal_id,
            goal, blocked_goal, blocked_frontier, tier,
            // #766: RETIRED, not kept. The fine tier's verdict is about threading toward the goal
            // that is now over — a `no_way_through` published beside `idle`/`zoned` asserts something
            // about a corridor in a zone we have left, computed against a collision grid that no
            // longer exists. See the "#766 moved `local`" paragraph above for why this does not
            // undo #382's ownership: only the `idle` row is affected, and this function does not
            // touch `LocalPlanner` — but note the effect is not merely cosmetic, because
            // `Walker::local_says_no_way_through` reads this same field back as a steering input.
            // Clearing it on `idle` clears that input too, which is what we want: on an `idle` row
            // there is no goal, so there is no corridor for it to be an opinion about.
            local,
            // #851: RETIRED. `stall` says "the walker has a route and is not executing it" — a
            // statement about the route committed for the goal that is now over. Left standing
            // beside `idle` it would assert a live wedge for a journey nobody is on. It is also
            // the E0027 net working as designed: this field could not be added without a decision
            // being recorded here.
            stall,
            // KEPT, deliberately — and this is the field the E0027 net was built for. A dead fine
            // worker is a fact about the WORKER, not about the goal that just ended, and it is
            // latched because that thread does not come back. Retiring a goal is not replacing a
            // worker, so nothing here has repaired anything; clearing it would tell an agent between
            // goals that its degraded steering had healed. (Scoped to the worker, not to the session
            // — round-6 review B12. `Walker::new` is the one writer that clears it. See the field's
            // own doc, both for that and for why it is a separate field rather than a carve-out in
            // the `local` arm above.)
            local_planner_dead: _keep_local_planner_dead,
        } = self;
        *state  = "idle".to_string();
        *reason = why.map(str::to_string);
        *goal   = None;
        *blocked_goal     = None;
        *blocked_frontier = None;
        *tier             = None;
        *local            = None;
        *stall            = None;
    }

    /// **Stamp a FRESH GOAL's row: the non-idle twin of [`NavStatus::retire_to_idle`]** (#851 review
    /// round 1, B1). `CommandState::stamp_new_goal`'s only non-idle route, reached with `pending` by
    /// `request_goto`, `request_follow` and `request_zone_cross`.
    ///
    /// Same argument, same construction, different destination: a new goal ends the previous one, so
    /// every fact that was ABOUT that goal goes with it — and the write is EXHAUSTIVE (no `..`), so a
    /// field added to [`NavStatus`] is an `error[E0027]` here until someone decides its fate.
    ///
    /// **This function exists because that net was built for `idle` only, and the gap was then
    /// walked into.** `stamp_new_goal` used to carry a flat assignment list for the non-idle route,
    /// with the `#732` comment directly above it recording that a flat list has no exhaustiveness and
    /// that a field had already been silently forgotten in it once (measured, with a throwaway
    /// `probe_route_len`). #851 then added `stall` to [`NavStatus`], decided it in `retire_to_idle`
    /// where the compiler forced the decision, and missed the flat list where nothing did — so a
    /// re-issued goal published `nav_state: "pending"` beside the DEAD goal's live `nav_stall`, which
    /// is #851's own failure shape one goal later and in the false-alarm direction. Fixing the field
    /// alone would have left the next per-goal field to be lost the same way; the fix is the net.
    ///
    /// `why` is `None` for every production caller today (a `pending` needs no explanation — the
    /// request itself is the explanation); it is a parameter rather than a hard-coded `None` because
    /// nothing about a fresh goal makes a reason wrong, and the `idle` twin proves reasons matter.
    /// `idle` is refused outright: that state is a RETIREMENT and has its own writer, and routing it
    /// through here would silently skip the `#725` "an `idle` must say how it got there" guard.
    pub fn stamp_fresh_goal(&mut self, new_state: &str, why: Option<&str>, goal: Option<[f32; 3]>) {
        debug_assert!(new_state != "idle",
            "#851 B1: `idle` is a retirement, not a fresh goal — use `NavStatus::retire_to_idle`");
        let NavStatus {
            state, reason,
            // KEPT — see `retire_to_idle`: `goal_id` is a monotonic IDENTITY stamp, and the caller
            // has already bumped it for this goal. Re-deciding it here would break #349's
            // correlation between a request and the row that answers it.
            goal_id: _keep_goal_id,
            goal: goal_slot,
            blocked_goal, blocked_frontier, tier, local, stall,
            // KEPT — a dead fine worker is a fact about the WORKER, not about any goal, and a new
            // goal has repaired nothing. Same argument as in `retire_to_idle`; see the field's doc.
            local_planner_dead: _keep_local_planner_dead,
        } = self;
        *state  = new_state.to_string();
        *reason = why.map(str::to_string);
        *goal_slot        = goal;
        *blocked_goal     = None;
        *blocked_frontier = None;
        *tier             = None;
        *local            = None;
        *stall            = None;
    }

    /// **Move to a new state WITHIN the same goal** — the walker's mid-route transition
    /// (`Walker::write_nav_state_locked`), and the third exhaustive writer of this row (#851 review
    /// round 1, B1). Exhaustive for the same reason as the other two: a field added to [`NavStatus`]
    /// is an `error[E0027]` here until its fate on a mid-route transition is decided.
    ///
    /// It keeps two things the goal-changing writers retire, and both are deliberate:
    ///   * `goal` — the goal has NOT changed. `planning` → `navigating` → `arrived` are all states
    ///     of the same journey, and clearing its coordinates mid-flight would be a fresh lie.
    ///   * `local` — #382: the fine tier's last word is an independent fact about a different tier
    ///     and is the EVIDENCE behind a terminal `blocked`/`no_path`. `retire_to_idle` clears it
    ///     because `idle` means the goal is over; a transition within the goal is not that.
    ///
    /// `idle` is refused here too, for the same reason as in [`NavStatus::stamp_fresh_goal`]: the
    /// caller routes it to `retire_to_idle` before reaching this.
    pub fn transition_within_goal(&mut self, new_state: &str, why: Option<&str>) {
        debug_assert!(new_state != "idle",
            "#851 B1: `idle` retires the goal — route it through `NavStatus::retire_to_idle`");
        let NavStatus {
            state, reason,
            goal_id: _keep_goal_id,
            // KEPT — the journey is the same one; see the doc above.
            goal: _keep_goal,
            blocked_goal, blocked_frontier, tier,
            // KEPT — #382; see the doc above.
            local: _keep_local,
            // #851: RETIRED. `stall` is a fact about executing the route under the state we are
            // LEAVING. `publish_drive_state` re-asserts it in the same lock hold when the new state
            // is still a driving one; anywhere else — `blocked`, `arrived`, `planning`,
            // `zone_loading` — it must go, or a terminal row would carry a live-wedge payload.
            stall,
            local_planner_dead: _keep_local_planner_dead,
        } = self;
        *state  = new_state.to_string();
        *reason = why.map(str::to_string);
        *blocked_goal     = None;
        *blocked_frontier = None;
        *tier             = None;
        *stall            = None;
    }
}

impl Default for NavStatus {
    fn default() -> Self {
        NavStatus { state: "idle".into(), reason: None, local: None, stall: None,
            blocked_goal: None, blocked_frontier: None, tier: None,
            goal_id: 0, goal: None, local_planner_dead: false }
    }
}

// `impl From<&str> for NavStatus` was deleted here (#771). It was a fourth, unnamed way to build a
// `NavStatus` alongside `NavStatus::default()` (the boot row) and the three writers
// `retire_to_idle` funnels every `idle` transition through — and unlike all three of those, it
// named no reason: `"idle".into()` minted `state: "idle", reason: None` directly, exactly the
// combination `CommandState::stamp_new_goal`'s `debug_assert` (`eqoxide-command/src/nav.rs:146`)
// exists to reject, reached without passing through it.
//
// #765's review found this and classed it non-blocking because production never called it. #771
// first tried `#[cfg(test)]`, but that gates something nothing can reach: its only two callers
// were `eqoxide-net::action_loop`'s test module, a DIFFERENT crate, and they were migrated to
// `NavStatus { state: "...".to_string(), ..Default::default() }` (see `action_loop.rs`) once bare
// `#[cfg(test)]` — which is per-crate — turned out not to reach them (Cargo builds a dependency in
// its normal mode while compiling the crate under test, not in test mode). Once those were gone, a
// repo-wide check — including `eqoxide-ipc`'s own test module — found ZERO remaining callers in
// any crate, in any build configuration. A gate on an unreachable impl is dead weight, not a
// guarantee. This codebase's actual idiom for a downstream-visible test fixture is
// `#[cfg(any(test, feature = "test-fixtures"))]`, used throughout this file — but this impl needed
// neither that nor bare `#[cfg(test)]`, because nothing anywhere used it. Deletion is the maximal
// form of "make the bad state unrepresentable": there is no longer any code path, gated or not,
// that can build a `NavStatus` this way. If a genuine downstream test need for this shorthand
// shows up later, it should be reintroduced under `#[cfg(any(test, feature = "test-fixtures"))]`,
// not bare `#[cfg(test)]`.

impl PartialEq<&str> for NavStatus {
    fn eq(&self, other: &&str) -> bool { self.state == *other }
}

pub type NavStateShared = Arc<Mutex<NavStatus>>;

/// Pending "click a dialogue choice" request (POST /v1/interact/dialogue or a GUI click): the nav
/// thread drains it and sends an OP_ItemLinkClick for the chosen saylink. (#120)
pub type DialogueClickReq = Arc<Mutex<Option<eqoxide_core::game_state::DialogueChoice>>>;

/// One async game event exposed by the `GET /v1/events/*` feed. `category` is the top-level bucket
/// the events API filters on (chat/combat/navigate/system); `kind` is the sub-type
/// (tell/ooc/shout/group/gmsay/zone/slain/attacked/…). `id` is a 1-based monotonic cursor;
/// `directed` = concerns us specifically (a /tell to our name, a GM message, a zone change, our own
/// death). Agents poll `/v1/events/{all,<category>}?since=<id>` (optionally long-poll with `wait=`).
#[derive(Clone, serde::Serialize)]
pub struct Event {
    pub id:       u64,
    pub category: String,
    pub kind:     String,
    pub from:     String,
    pub directed: bool,
    pub text:     String,
}

/// Live snapshot of async events, published each tick by the nav thread, read by the
/// `GET /v1/events/*` endpoints. Ordered by ascending `id`.
pub type ChatEventsShared = Arc<Mutex<Vec<Event>>>;

/// One queued outgoing chat message, set by POST /v1/chat/{tell,ooc,shout,group} and drained by the
/// nav thread, which builds + sends the `OP_ChannelMessage`. `to` is the recipient for /tell (chan
/// 7), empty for broadcasts. `chan` is the EQ ChatChannel number.
#[derive(Clone)]
pub struct ChatSend {
    pub chan: u32,
    pub to:   String,
    pub text: String,
}

/// Outgoing chat queue (FIFO), written by the /v1/chat/{tell,ooc,shout,group} endpoints.
pub type ChatSendShared = Arc<Mutex<Vec<ChatSend>>>;

#[derive(Clone, Copy)]
pub struct CastRequest {
    pub gem: u8,
    pub target_id: Option<u32>,
    /// When Some, this is an item "clicky" cast: the wire inventory slot of the item to activate.
    /// The gem field is then ignored and the click spell is resolved from the item. (eqoxide#193)
    pub item_slot: Option<u32>,
}
/// Cast a memorized gem (0-8) on an explicit target, else current target, else self.
/// This is the FIRE-AND-FORGET cast the UI spell-gem click uses; the honest awaited variant
/// (POST /v1/combat/cast over HTTP) rides the sibling [`CastAwaitReq`] instead. (#448)
pub type CastReq = Arc<Mutex<Option<CastRequest>>>;

/// Command-with-result cast request (A3 Migration 3, #448) — `(CastRequest, oneshot Sender)`. POST
/// /v1/combat/cast writes this and AWAITS the `Sender`; the nav thread drains it, emits the SAME
/// OP_CastSpell the fire-and-forget [`CastReq`] path sends, and PARKS the `Sender` in
/// `ActionLoop::pending_cast` until the cast's TRUE outcome is known. The cast outcome is already
/// computed by the existing cast machinery into `gs.last_cast` (completed / fizzled / interrupted /
/// failed) — the net thread fulfils by detecting that `last_cast` TRANSITION (NOT a single opcode:
/// the 3-opcode cast-end path is deliberately de-duped, so keying one opcode would double-fire or
/// miss). A cast that never started (empty gem / stale clicky) fires `Refused` immediately from the
/// drain; a truly silent cast resolves to `Unconfirmed` via the HTTP timeout / a zone-change reaper.
/// Sibling of [`CastReq`], NOT a replacement: the UI click path is unchanged. One self-cast at a
/// time → a singleton park suffices. See [`result`] for the flow.
pub type CastAwaitReq = Arc<Mutex<Option<(CastRequest,
    oneshot::Sender<CommandResult<CastEnd>>)>>>;
/// Scribe/memorize request — (slot, spell_id, scribing): scribing 0 = scribe a scroll into the
/// spellbook at book `slot`; 1 = memorize a known spell into gem `slot` (0-8). Set by POST
/// /v1/combat/scribe and POST /v1/combat/memorize; the nav thread sends OP_MemorizeSpell.
/// Tuple = `(slot, spell_id, scribing, from_slot)`. `from_slot` is only used for scribing (0): the
/// RoF2 server scribes only the scroll on the CURSOR, so the nav thread first moves the scroll from
/// `from_slot` → cursor (OP_MoveItem) before the scribe packet. `None` = scroll already on cursor
/// (or memorize/un-mem, which need no move). See eqoxide#11.
pub type MemSpellReq = Arc<Mutex<Option<(u32, u32, u32, Option<u32>)>>>;
/// Posture: Some(true)=sit, Some(false)=stand.
pub type SitReq = Arc<Mutex<Option<bool>>>;
/// Run/walk toggle (#625): `Some(true)` = run, `Some(false)` = walk. Set by the Actions window's
/// Run/Walk button and POST /v1/interact/{run,walk}; the drain sends `OP_SetRunMode` (0x009f) and
/// selects the local movement speed (see `eqoxide_core::physics::WALK_SPEED`).
pub type RunModeReq = Arc<Mutex<Option<bool>>>;
/// Standalone consider of a spawn id.
pub type ConsiderReq = Arc<Mutex<Option<u32>>>;

/// Door-click request — a door_id, set by POST /v1/interact/click_door or a human click in the 3D
/// view. The nav thread reads it once and sends OP_ClickDoor. The door's visual state changes only
/// when the server replies with OP_MoveDoor (server-authoritative).
pub type DoorClickReq = Arc<Mutex<Option<u8>>>;

#[derive(Clone, serde::Serialize)]
pub struct DoorView {
    pub door_id:  u8,
    pub name:     String,
    pub x:        f32,
    pub y:        f32,
    pub z:        f32,
    pub heading:  f32,
    pub opentype: u8,
    pub is_open:  bool,
}
/// Snapshot of the current zone's doors, published each nav tick for GET /v1/observe/doors.
pub type DoorsShared = Arc<Mutex<Vec<DoorView>>>;

/// Current zone name and id, updated on every OP_NEW_ZONE.
#[allow(dead_code)]
pub type ZoneInfo = Arc<Mutex<(String, u16)>>;

// ── Domain slot bundles (M4) ────────────────────────────────────────────────────────────────
//
// Everything above this line is an individual slot alias/type. `ActionLoop` (the network/nav
// thread's per-tick state, `eq_net::action_loop`) and `HttpState` (the HTTP API's per-request
// state, `http::mod`) each used to hold ~50–60 of these as flat, individually-named fields —
// duplicated field lists in two structs, two constructors, and two hand-written test builders,
// with no structure connecting e.g. `attack`/`cast`/`target` as "the combat slots" beyond
// eyeballing the source.
//
// These bundles regroup the same fields BY DOMAIN, one struct per HTTP API group
// (`/v1/combat`, `/v1/merchant`, `/v1/group`, …) — the router nesting in `http::mod::
// spawn_camera_server` is the authoritative domain boundary these mirror, since that's already
// the seam a future shared "controller verb" (one call both a UI click-handler and an agent HTTP
// handler go through, instead of each independently poking a slot) would need to land on. This
// is PURE REGROUPING: every field keeps its original name and `Arc`-sharing semantics unchanged
// — only its home moved from `ActionLoop`/`HttpState` directly to one of these, embedded by
// whichever of the two structs actually reads it. See `ActionLoop::new` and
// `http::mod::spawn_camera_server`/`HttpState` for how a bundle is constructed exactly ONCE and
// then `.clone()`d (a shallow `Arc`-handle clone, not a fresh channel) into each consumer that
// needs it — never `Default`-constructed twice, which would silently sever the channel.
//
// A `TODO(MVC)` marker sits at a handful of representative drain sites in `action_loop.rs` for
// where that future controller-verb unification would land; these bundles are the plumbing for
// it, not the verbs themselves (out of scope here — this is a behavior-preserving refactor).

/// `/v1/combat/*`: targeting, auto-attack, consider, spell scribe/memorize/cast, and the one
/// `/v1/pet/command` slot (small enough on its own that a dedicated `PetSlots` would just be
/// noise — it rides along with the other "act on a target" verbs).
#[derive(Clone, Default)]
pub struct CombatSlots {
    pub attack:   AttackReq,
    pub cast:     CastReq,
    /// The honest awaited-cast slot (A3 Migration 3, #448) — sibling of `cast`. See [`CastAwaitReq`].
    pub cast_await: CastAwaitReq,
    pub mem_spell: MemSpellReq,
    pub consider: ConsiderReq,
    pub target:   TargetReq,
    pub pet_cmd:  PetCmdReq,
}

/// `/v1/merchant/*`: open/close a vendor window, list wares, buy, sell.
#[derive(Clone, Default)]
pub struct MerchantSlots {
    pub merchant: MerchantShared,
    pub buy:      BuyReq,
    /// The honest awaited-buy slot (A3 Migration 1, #448) — sibling of `buy`. See [`BuyAwaitReq`].
    pub buy_await: BuyAwaitReq,
    pub sell:     SellReq,
    pub trade:    TradeReq,
    /// The honest awaited-open slot (eqoxide#479) — sibling of `trade`. See [`OpenAwaitReq`].
    pub open_await: OpenAwaitReq,
}

/// `/v1/inventory/*`: the live snapshot plus the one move/equip/unequip request slot.
#[derive(Clone, Default)]
pub struct InventorySlots {
    pub inventory: InventoryShared,
    pub move_req:  MoveReq,
}

/// `/v1/interact/*`: NPC/world interaction — hail, say, loot, give (turn-in), doors, sit/stand,
/// dialogue clicks, and reading a book/note. Mirrors `http::interact`'s own module doc verbatim
/// ("NPC/world interaction: hail, say, loot, give (turn-in), doors, sit/stand") — that file is
/// the domain boundary this bundle reifies, including `doors_shared` (the read-side twin of
/// `door_click`, published for GET /v1/observe/doors but conceptually the same door verb).
#[derive(Clone, Default)]
pub struct InteractSlots {
    pub hail:           HailReq,
    pub say:            SayReq,
    pub loot:           LootReq,
    pub give:           GiveReq,
    /// The honest awaited-give slot (A3 Migration 2, #448) — sibling of `give`. See [`GiveAwaitReq`].
    pub give_await:     GiveAwaitReq,
    pub door_click:     DoorClickReq,
    pub doors_shared:   DoorsShared,
    pub sit:            SitReq,
    /// #625 run/walk toggle. Grouped with `sit` (both are posture/mode toggles the Actions window
    /// exposes) rather than a new `NavSlots` field, to avoid touching the nav command domain.
    pub run_mode:       RunModeReq,
    pub dialogue:       DialogueShared,
    pub dialogue_click: DialogueClickReq,
    pub read_book:      ReadBookReq,
}

/// `/v1/quests/*`: the native Task-system log/offers/history plus accept/cancel requests.
#[derive(Clone, Default)]
pub struct QuestSlots {
    pub task_log:               TaskLog,
    pub task_offers_shared:     TaskOffersShared,
    pub completed_tasks_shared: CompletedTasksShared,
    pub accept_task:            AcceptTaskReq,
    pub cancel_task:             CancelTaskReq,
}

/// `/v1/group/*`: roster + invite/accept/decline/leave/kick/transfer-leadership.
#[derive(Clone, Default)]
pub struct GroupSlots {
    pub group:             GroupShared,
    pub group_invite:      GroupInviteReq,
    pub group_accept:      GroupAcceptReq,
    pub group_decline:     GroupDeclineReq,
    pub group_leave:       GroupLeaveReq,
    pub group_kick:        GroupKickReq,
    pub group_make_leader: GroupMakeLeaderReq,
}

/// `/v1/guild/*`: roster + identity snapshot plus the one queued guild action.
#[derive(Clone, Default)]
pub struct GuildSlots {
    pub guild:        GuildShared,
    pub guild_action: GuildActionReq,
}

/// `/v1/trainer/*`: open a trainer window, train a skill.
#[derive(Clone, Default)]
pub struct TrainerSlots {
    pub trainer_open_req:  TrainerOpenReq,
    pub trainer_train_req: TrainerTrainReq,
}

/// `/v1/social/*`: the client-local friends list plus the `/who` and friends-presence polls.
#[derive(Clone, Default)]
pub struct SocialSlots {
    pub who_req:      WhoReq,
    pub friends_list: FriendsListShared,
    pub friends_req:  FriendsReq,
}

/// The outgoing/async text feeds: `/v1/chat/*` (outgoing), `/v1/events/*` (async feed), and the
/// machine-readable NPC/system message log surfaced at `/v1/observe/messages`. Grouped together
/// (rather than splitting `messages` into its own bundle or into `InteractSlots`) because all
/// three are "a queue/log of text the nav thread produces or consumes", read by adjacent handlers
/// in practice (an agent polling `/events` is usually also reading `/observe/messages`).
#[derive(Clone, Default)]
pub struct ChatSlots {
    pub chat_events: ChatEventsShared,
    pub chat_send:   ChatSendShared,
    pub messages:    MessagesShared,
}

/// `/v1/move/*`: the `/goto` target (+ chase-entity), zone-crossing, aggro-avoidance knobs, and live
/// nav status. Does NOT include the manual-move/jump escape hatch (`ManualMoveReq`) — that slot is
/// consumed by the RENDER thread, not the nav thread/`ActionLoop` (see `CameraSlots`), so folding it
/// in here would make `ActionLoop` carry a field it can never read.
///
/// MVC C2 (#452): the walker's draw-only computed path (`nav_path_view`) was moved OUT of here to
/// [`ControllerSlots`] — it is Model→View derived render state, not a view→model command, so it does
/// not belong in a command bundle carried by `command_state::CommandState`.
#[derive(Clone, Default)]
pub struct NavSlots {
    pub goto_target:   GotoTarget,
    pub goto_entity:   GotoEntity,
    pub zone_cross:    ZoneCrossReq,
    pub nav_avoid:     NavAvoidShared,
    pub nav_state:     NavStateShared,
}

/// The live entity registry (`login.rs` writes it as spawn packets arrive): name → position/id,
/// plus the zone's exit points. Read by nearly every domain to resolve a name/target to a spawn
/// id (merchant buy/sell, combat target, trainer open, `/goto` by name, …) — it is genuinely a
/// shared world index, not particular to navigation, even though nav is its biggest reader.
#[derive(Clone)]
pub struct WorldSlots {
    // The three roster maps are PRIVATE (#665): a `pub Arc<Mutex<Roster<..>>>` field hands out a
    // `MutexGuard`, whose `DerefMut` yields a `&mut Roster` that `mem::swap` can move past the
    // single-publisher rule (`publish_entities`). Reads go through the [`RosterReadGuard`] accessors
    // below (no `DerefMut`); the only writer is `publish_entities`. #652 sealed the VALUE producers;
    // this seals the CONTAINER.
    entity_positions: EntityPositions,
    entity_ids:       EntityIds,
    /// name → pose/gait (#643). Same keys as `entity_positions`; published by the same
    /// `sync_entities` full-replace so it can never go stale independently of the roster.
    entity_poses:     EntityPoses,
    pub zone_points:      ZonePoints,
    /// #816 — see [`ZoneMapLoadShared`].
    pub zone_map_load:    ZoneMapLoadShared,
}

// Hand-written rather than `#[derive(Default)]`: `Roster` deliberately has NO public constructor
// (#643 — see its doc comment), so only this crate can build the empty maps. A derive would have
// required `Roster: Default`, which is exactly the public value-producer that let an outside crate
// assign a whole roster through the guard and bypass the single-publisher rule.
impl Default for WorldSlots {
    fn default() -> Self {
        WorldSlots {
            entity_positions: Arc::new(Mutex::new(Roster::new())),
            entity_ids:       Arc::new(Mutex::new(Roster::new())),
            entity_poses:     Arc::new(Mutex::new(Roster::new())),
            zone_points:      Arc::new(Mutex::new(Vec::new())),
            zone_map_load:    Arc::new(Mutex::new(None)),
        }
    }
}

impl WorldSlots {
    /// **The one and only way to publish the entity roster.** Full-replaces `entity_positions`,
    /// `entity_ids` and `entity_poses` from `entities`, holding all three locks for the whole
    /// swap. Returns the number of entities published.
    ///
    /// # Why this exists (#643 review round 2)
    ///
    /// `/v1/observe/entities?labeled=1` promises that `poses` is keyed EXACTLY like `entities`, so
    /// an agent may write `body["poses"][name]` without a `KeyError`. That promise is only as good
    /// as its weakest publisher, and it was already broken once: this crate has **two** roster
    /// publishers — `eqoxide_net::action_loop::sync_entities` (every nav tick) and
    /// `eqoxide_net::login`'s zone-in seed — and when `entity_poses` was added, only the first one
    /// was updated. The seed kept writing positions and ids without poses, so every entity's
    /// `poses` key was missing for the whole window between login and the first nav tick.
    ///
    /// The first fix was to add the missing lines to the second loop. That left the invariant as a
    /// *convention duplicated across two hand-written loops*, which a reviewer falsified by simply
    /// deleting the new lines again: the entire workspace suite stayed green. A third publisher
    /// would reintroduce the bug by omission exactly as the second one did.
    ///
    /// So the invariant moved into a type, next to the fields it constrains: there is now one
    /// function that writes these maps, it cannot write one without the others, and a new publisher
    /// gets the guarantee by construction rather than by remembering. (Same move as `Pose`/`Gait`
    /// in `eqoxide-core`, one level up: make the broken state unrepresentable rather than
    /// documenting a rule and hoping.)
    ///
    /// A source-scanning test was tried here first and a reviewer defeated it in one line — see
    /// [`Roster`], which now makes a second publisher a COMPILE ERROR instead.
    ///
    /// # ⚠️ Lock order
    ///
    /// `entity_positions` → `entity_ids` → `entity_poses`. This is the canonical order every other
    /// site must follow (see `eqoxide_http::name_match`'s `resolve_in_world` and its ABBA regression
    /// guard). Centralising the write path here means the *writer* half of that discipline now
    /// exists in exactly one place and cannot drift.
    ///
    /// # Full replace, deliberately
    ///
    /// Both callers want current-zone truth, so stale entries from a previous zone are cleared
    /// rather than merged. `sync_entities` already did this; the login seed did not, and inherits
    /// the stricter behaviour here.
    ///
    /// For the login seed this is **latent hardening, not a bug that was reachable**: on current
    /// control flow the seed runs exactly once against still-empty maps (it sits in the `Ok(..)`
    /// arm, so a failed attempt never seeds and a successful one never returns to the retry loop),
    /// and `sync_entities` full-replaces from authoritative state on the next tick regardless. An
    /// earlier revision of this PR described it as a second live bug; that was an overclaim.
    pub fn publish_entities<'a, I>(&self, entities: I) -> usize
    where
        I: IntoIterator<Item = (&'a u32, &'a eqoxide_core::game_state::Entity)>,
    {
        let mut positions = self.entity_positions.lock().unwrap(); // 1st — canonical order
        let mut ids       = self.entity_ids.lock().unwrap();       // 2nd
        let mut poses     = self.entity_poses.lock().unwrap();     // 3rd
        positions.clear();
        ids.clear();
        poses.clear();
        for (&id, e) in entities {
            positions.insert(e.name.clone(), (e.x, e.y, e.z));
            ids.insert(e.name.clone(), id);
            poses.insert(e.name.clone(), EntityPoseView {
                pose: e.pose.label(),
                gait: e.gait.map(|g| g.raw()),
            });
        }
        positions.len()
    }

    /// Read-lock the live **positions** roster (name → `(x, y, z)`). Reads only — the returned
    /// [`RosterReadGuard`] has no `DerefMut`, so it cannot be swapped or otherwise written; writes
    /// go through [`publish_entities`](Self::publish_entities) alone (#665).
    ///
    /// # ⚠️ Lock order
    ///
    /// A site holding more than one of these must acquire them in the canonical order
    /// `entity_positions()` → `entity_ids()` → `entity_poses()` — the same order `publish_entities`
    /// and `eqoxide_http::name_match`'s ABBA guard use. Taking them any other way is a deadlock.
    ///
    /// # The #665 leak is now a compile error
    ///
    /// The original witness moved a whole roster with `mem::swap`, which needs two `&mut Roster`.
    /// The private field can't be reached and this accessor yields no `&mut`, so both forms fail to
    /// compile:
    ///
    /// ```compile_fail
    /// // Form 1 — the field is private; `.lock()` is unreachable from outside the crate.
    /// let world = eqoxide_ipc::WorldSlots::default();
    /// let _g = world.entity_positions.lock().unwrap();
    /// ```
    ///
    /// ```compile_fail
    /// // Form 2 — the read accessor yields no `&mut Roster`, so there is nothing to `mem::swap`.
    /// let world = eqoxide_ipc::WorldSlots::default();
    /// let scratch = eqoxide_ipc::WorldSlots::default();
    /// std::mem::swap(
    ///     &mut *world.entity_positions(),
    ///     &mut *scratch.entity_positions(),
    /// );
    /// ```
    pub fn entity_positions(&self) -> RosterReadGuard<'_, (f32, f32, f32)> {
        RosterReadGuard(self.entity_positions.lock().unwrap())
    }

    /// Read-lock the live **ids** roster (name → spawn id). Reads only; see
    /// [`entity_positions`](Self::entity_positions) for the lock order and the #665 rationale.
    pub fn entity_ids(&self) -> RosterReadGuard<'_, u32> {
        RosterReadGuard(self.entity_ids.lock().unwrap())
    }

    /// Read-lock the live **poses** roster (name → [`EntityPoseView`]). Reads only; see
    /// [`entity_positions`](Self::entity_positions) for the lock order and the #665 rationale.
    pub fn entity_poses(&self) -> RosterReadGuard<'_, EntityPoseView> {
        RosterReadGuard(self.entity_poses.lock().unwrap())
    }

    /// **Test fixtures only.** Mutable lock on the **positions** roster, so a test can seed a partial
    /// or deliberately-mismatched roster via [`Roster::insert_for_test`]. Gated to `test` /
    /// `test-fixtures`, so it is **absent from `cargo build --release`** — the release build keeps no
    /// mutable path to these maps outside `publish_entities`, which is what makes the #665 leak a
    /// compile error there rather than a runtime convention.
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn entity_positions_mut(&self) -> std::sync::MutexGuard<'_, Roster<(f32, f32, f32)>> {
        self.entity_positions.lock().unwrap()
    }

    /// **Test fixtures only.** Mutable lock on the **ids** roster; see
    /// [`entity_positions_mut`](Self::entity_positions_mut).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn entity_ids_mut(&self) -> std::sync::MutexGuard<'_, Roster<u32>> {
        self.entity_ids.lock().unwrap()
    }

    /// **Test fixtures only.** Mutable lock on the **poses** roster; see
    /// [`entity_positions_mut`](Self::entity_positions_mut).
    #[cfg(any(test, feature = "test-fixtures"))]
    pub fn entity_poses_mut(&self) -> std::sync::MutexGuard<'_, Roster<EntityPoseView>> {
        self.entity_poses.lock().unwrap()
    }
}

/// Single-authority controller integration (design §2): the render thread's authoritative
/// position snapshot streamed to the server, the `/goto` planner's per-frame movement intent, and
/// a server correction handed back to the controller. `ActionLoop`-only — `HttpState` has no
/// controller-facing endpoint today, so there is nothing for it to embed here.
///
/// (#608: the walker's draw-only `nav_path_view` overlay pair that #452 moved here is gone — the
/// walker now publishes the full `eqoxide_nav::diagnostics::NavDebugSnapshot`, whose view slot is
/// defined in `eqoxide-nav` because this crate sits below it. See the note above
/// `EntityPositions`.)
#[derive(Clone, Default)]
pub struct ControllerSlots {
    pub controller_view: ControllerShared,
    pub nav_intent:      NavIntent,
    pub pos_correction:  PosCorrection,
}

impl ControllerSlots {
    /// **The zone-in clear, whole (#846 review B1).** Use this on the net thread instead of calling
    /// [`eqoxide_core::game_state::GameState::begin_zone_in`] directly.
    ///
    /// `GameState::begin_zone_in` clears the two disclosure FIELDS; this also invalidates the
    /// controller view they are mirrored FROM. Doing only the first is what #846's round-1 review
    /// measured: `begin_zone_in` set `player_hold = None`, and the next `ActionLoop::stream_position`
    /// tick — an unconditional mirror of `ControllerView::disclosures()`, running ~every 10 ms
    /// whether or not the render loop is awake — put the departed zone's
    /// `Some(EmbeddedNoRecovery, 7.5)` straight back. The clear survived about one net tick, so the
    /// case its own doc says it covers (the render loop publishes *nothing at all* across a zone
    /// load) was the exact case it did not cover.
    ///
    /// The two clears live in one function because they are one act, and because separating them is
    /// silent: the `GameState` half alone leaves a well-formed, confidently-served
    /// `player.hold` about a zone the character has left, which is #846's shape and #343's shape.
    /// This does not make the pairing *unrepresentable* — `GameState::begin_zone_in` is still `pub`
    /// and eqoxide-core cannot reach this crate (it sits below it), so a new net-thread caller can
    /// still call the half — it makes the whole act reachable by one name and puts the reasoning at
    /// it. `GameState::begin_zone_in`'s own doc points here.
    ///
    /// Deliberately does NOT touch `ControllerView::pos`, `heading` or `initialized`: this crate
    /// does not own the controller's placement, and blanking `initialized` here would only move the
    /// stale-position window from "one net tick" to "one render frame" rather than close it (that
    /// residual is `player_pos_known`'s, and is filed as #871 rather than fixed here — a different
    /// field, whose only writer is the same unconditional mirror, but whose blast radius is position
    /// streaming).
    pub fn begin_zone_in(&self, gs: &mut eqoxide_core::game_state::GameState) {
        gs.begin_zone_in();
        self.controller_view.lock().unwrap().invalidate_disclosures();
    }
}

/// `/v1/lifecycle/*`: camp (+ its published deadline) and respawn. `HttpState`-only: `ActionLoop`
/// only ever WRITES `camp` (never reads `camp_until`/`respawn`, which the separate gameplay-tick
/// gets directly — see `eq_net::gameplay::run_gameplay_phase`), so it keeps a lone `camp` field
/// rather than embedding this whole bundle for one field it partially uses.
#[derive(Clone, Default)]
pub struct LifecycleSlots {
    pub camp:       CampReq,
    pub camp_until: CampUntil,
    pub respawn:    RespawnReq,
}

/// What HTTP hands straight to the RENDER thread, bypassing the nav thread entirely:
/// `/v1/camera/*` (cmd + published snapshot), `GET /v1/observe/frame` (frame-capture request),
/// and the manual-move/jump escape hatch consumed by the controller alongside WASD. `HttpState`-
/// only; no `Default` (the camera snapshot's initial value is meaningful — see `App::new`/
/// `main.rs` — so callers construct this explicitly rather than risk a silently-wrong default).
///
/// MVC C2 (#452): the manual-move/jump escape hatch (`manual_move`) is a view→RENDER command — the
/// render thread's controller consumes it (see `App`), the Model/nav thread never does — so it lives
/// HERE, on the render-bound camera bundle, and NOT in the view→MODEL `command_state::CommandState`
/// facade. `request_manual_move` is the typed write the HTTP View makes (mirroring
/// `cmd_tx`'s role for the orbit camera); the render View reads `manual_move` directly per frame.
#[derive(Clone)]
pub struct CameraSlots {
    pub cmd_tx:      Arc<Mutex<Option<CameraCmd>>>,
    pub snapshot:    Arc<Mutex<CameraSnapshot>>,
    pub frame_req:   FrameReq,
    pub manual_move: ManualMoveReq,
}

impl CameraSlots {
    /// Queue a manual-move/jump escape-hatch command (POST /v1/move/manual, /v1/move/jump). The
    /// render thread's `CharacterController` picks it up next frame and drives until `m.until`
    /// (#188/#207). This is a view→render command; it never reaches the Model/nav thread.
    pub fn request_manual_move(&self, m: ManualMove) {
        *self.manual_move.lock().unwrap() = Some(m);
    }
}

/// MVC C2 (#452): pin the tidied CommandState boundary at the `ipc` layer.
#[cfg(test)]
mod zone_in_disclosure_tests {
    use super::*;
    use eqoxide_core::game_state::{ControllerHold, ControllerHoldReason, GameState};

    /// **#846 review B1 — the two halves of a zone-in clear, pinned together.**
    ///
    /// `GameState::begin_zone_in` clears the two disclosure FIELDS; it cannot reach the
    /// `ControllerView` they are mirrored from, because `eqoxide-core` sits below this crate.
    /// Clearing only the fields is what round 1 of this PR's review measured: the departed zone's
    /// hold was back one `ActionLoop::stream_position` tick later, because that mirror is
    /// unconditional and the view still held it. So the pairing is the unit, and this is the test
    /// of the unit.
    ///
    /// MUTATION CHECKS (#846, each run independently, results in the PR body):
    /// 1. drop `self.controller_view.lock().unwrap().invalidate_disclosures();` from
    ///    `ControllerSlots::begin_zone_in` (the `gs.begin_zone_in()` call left written and
    ///    executing — a WRAP mutation per #799) → RED at the view assertions;
    /// 2. drop `gs.begin_zone_in()` from it → RED at the GameState assertions;
    /// 3. **half-neuter it** — `let keep = self.disclosures().1; self.publish_disclosures((None,
    ///    keep));` — so the hold is invalidated and the stall is not → RED at the stall assertion.
    ///    Mutation 3 was **workspace-GREEN** before #846's round-2 revision, because every fixture
    ///    in this crate and in `eqoxide-net` published `(Some(hold), None)`, which left every
    ///    stall assertion satisfied by `GameState::begin_zone_in`'s own field clear and therefore
    ///    unfalsifiable. Hence [`matured_stall`] below: the stall axis has to be REACHED, not just
    ///    asserted (#778's lesson, applied to an axis rather than a branch).
    #[test]
    fn controller_slots_begin_zone_in_clears_both_the_copy_and_the_source() {
        let slots = ControllerSlots::default();
        let mut gs = GameState::new();

        const IN_THE_OLD_ZONE: [f32; 3] = [-812.5, 43.0, -119.75];
        let hold = ControllerHold { reason: ControllerHoldReason::EmbeddedNoRecovery, secs: 7.5 };
        let stall = matured_stall(IN_THE_OLD_ZONE);
        slots.controller_view.lock().unwrap().publish_disclosures((Some(hold), Some(stall)));
        gs.player_hold = Some(hold);
        gs.player_afloat_stall = Some(stall);
        gs.player_pos_known = true;

        slots.begin_zone_in(&mut gs);

        assert!(gs.player_hold.is_none(),
            "the GameState copy must be cleared — a hold describes geometry the zone-in dropped");
        assert!(gs.player_afloat_stall.is_none(),
            "and so must the stall copy, which is worse when stale: it names an ANCHOR in the \
             departed zone's coordinate frame");
        assert!(!gs.player_pos_known,
            "and the rest of `GameState::begin_zone_in` must still run: this method WRAPS it, it \
             does not replace it");
        assert_eq!(slots.controller_view.lock().unwrap().disclosures(), (None, None),
            "and the SOURCE must be cleared too — BOTH halves of it — or the next unconditional \
             mirror in `ActionLoop::stream_position` puts the departed zone's disclosures straight \
             back; measured to happen on the very next net tick (#846 review B1). The stall this \
             fixture publishes is a real matured one, so the second element of this tuple is a \
             live assertion rather than a `None == None` tautology (review F1).");
    }

    /// The invalidation must not latch the disclosures OFF: the render thread's first publication
    /// in the NEW zone has to come through. A zone-in that permanently silenced `player.hold` would
    /// trade a stale wedge alarm for a missing one, which is the same class of harm in the other
    /// direction.
    #[test]
    fn invalidating_disclosures_does_not_latch_them_off() {
        let mut view = ControllerView::default();
        let hold = ControllerHold { reason: ControllerHoldReason::UnderworldNoRecovery, secs: 0.5 };

        view.invalidate_disclosures();
        assert_eq!(view.disclosures(), (None, None));

        let stall = matured_stall([117.0, -8.25, 42.5]);
        view.publish_disclosures((Some(hold), Some(stall)));
        assert_eq!(view.disclosures(), (Some(hold), Some(stall)),
            "the render thread must still be able to publish BOTH disclosures after a zone-in");
    }

    /// Mature a real [`eqoxide_core::afloat::AfloatStall`] the only way any crate outside
    /// `eqoxide-core` can: real `Wished` frames at a fixed position until the clock matures.
    ///
    /// #800/#801 made a premature or fabricated stall unrepresentable outside its defining module —
    /// no `Default`, private fields, no way to edit an obtained one — and
    /// `crates/eqoxide-core/tests/afloat_unconstructible.rs` pins that from across a crate
    /// boundary. This helper is the consequence: getting the stall axis into a fixture costs a few
    /// simulated frames, which is exactly why the fixtures above did not have it until #846's
    /// round-2 review measured what that cost the tests.
    fn matured_stall(pos: [f32; 3]) -> eqoxide_core::afloat::AfloatStall {
        use eqoxide_core::afloat::{AfloatFrame, AfloatStallClock, AFLOAT_STALL_SECS};
        const DT: f32 = 0.016;
        let mut clock = AfloatStallClock::default();
        // `+ 3`: the first `Wished` frame opens the window at `secs = 0.0` and adds no time, and
        // f32 accumulation can cost another frame — the clock errs toward silence.
        for _ in 0..((AFLOAT_STALL_SECS / DT).ceil() as usize + 3) {
            clock.observe(AfloatFrame::Wished, pos, DT);
        }
        clock.stall().expect(
            "a body pinned at one point under a sustained wish must stall — if this returns None \
             the fixtures above go blind on the afloat axis again (#846 review F1)")
    }
}

#[cfg(test)]
mod c2_boundary_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The manual-move/jump escape hatch is a view→RENDER command owned by `CameraSlots` (the
    /// render-bound bundle), not the view→model `CommandState`. Round-trip its typed write against a
    /// direct per-frame read, exactly as the HTTP View writes it and the render View consumes it.
    #[test]
    fn camera_slots_manual_move_round_trips() {
        // Build a plain snapshot directly — the `camera_state::CameraState` that normally produces
        // one lives in the app crate (this crate owns only the `CameraSnapshot` type), and the
        // manual-move round-trip under test does not depend on the snapshot's contents.
        let camera = CameraSlots {
            cmd_tx:      Arc::new(Mutex::new(None)),
            snapshot:    Arc::new(Mutex::new(CameraSnapshot {
                mode:          CameraMode::AutoFollow,
                azimuth:       0.0,
                elevation:     0.0,
                radius:        0.0,
                focus:         [0.0, 0.0, 0.0],
                eye:           [0.0, 0.0, 0.0],
                occluded:      false,
                still_blocked: false,
                drawn_frame:   None,   // no frame drawn — a fixture, not a rendered snapshot
                drawn_at:      None,
            })),
            frame_req:   Arc::new(Mutex::new(None)),
            manual_move: Arc::new(Mutex::new(None)),
        };
        assert!(camera.manual_move.lock().unwrap().is_none());

        let m = ManualMove { dir: [1.0, 0.0], up: 0.0, jump: false, until: Instant::now() + Duration::from_millis(400) };
        camera.request_manual_move(m);
        // The render thread's per-frame read (see `App`): a non-clearing poll of `Option<ManualMove>`.
        let seen = camera.manual_move.lock().unwrap().expect("manual move queued");
        assert_eq!(seen.dir, [1.0, 0.0]);
    }

    /// #608: the walker's path overlay no longer flows through `ControllerSlots` at all — the
    /// published `NavDebugSnapshot` (in `eqoxide-nav`) is the ONE channel for committed routes.
    /// This pins that the controller bundle stayed a pure movement-integration channel.
    #[test]
    fn controller_slots_carry_only_movement_integration() {
        let controller = ControllerSlots::default();
        assert!(controller.nav_intent.lock().unwrap().is_none());
        assert!(controller.pos_correction.lock().unwrap().is_none());
    }
}

/// #371: the active-liveness-probe state machine, tested as a pure function. These are the exact
/// distinctions the issue turns on — a wedged-but-ACKing world vs a genuinely idle one — proved
/// without a socket. The `secs`/`ms` helpers keep the age arithmetic readable.
#[cfg(test)]
mod world_responsive_tests {
    use super::{world_responsive, PASSIVE_LIVENESS_STALE_SECS, PROBE_TIMEOUT_SECS};
    use std::time::Duration;

    const TIMEOUT: Duration = Duration::from_secs(PROBE_TIMEOUT_SECS);
    const STALE:   Duration = Duration::from_secs(PASSIVE_LIVENESS_STALE_SECS);
    fn s(secs: u64) -> Duration { Duration::from_secs(secs) }

    /// Shorthand for the #371 probe-path tests, which all assume a LIVE link (`connected == true`)
    /// and the standard bounds — those cases are about the probe verdict, not the link. The #470
    /// tests that vary `connected`/staleness call `world_responsive` in full.
    fn wr(first_unanswered_sent_ago: Option<Duration>, probe_reply_ago: Option<Duration>,
          last_packet_ago: Duration) -> bool {
        world_responsive(true, first_unanswered_sent_ago, probe_reply_ago, last_packet_ago, TIMEOUT, STALE)
    }

    /// THE bug (#371): a probe was sent, no reply has come, and the world has been silent longer than
    /// the bound — while the link is still ACKing. That is a wedged world, and it MUST read as such.
    #[test]
    fn unanswered_probe_past_the_bound_reports_the_world_wedged() {
        // The realistic wedge: the last spontaneous packet PREDATES the probe (world went quiet at
        // 30s ago, we probed 15s ago), the probe was never answered, and 15s > the 10s bound. The
        // probe is only ever sent AFTER a stretch of app-silence, so last_packet_ago > probe_sent_ago
        // always holds here — nothing has arrived since the probe to prove liveness.
        assert!(!wr(Some(s(15)), None, s(30)),
            "an unanswered probe past the timeout, on a still-ACKing link, is a wedged world");
    }

    /// The #343-trap-in-reverse: a legitimately IDLE session that has no spontaneous app traffic for
    /// 45s but whose probe IS answered must STILL read as live. This is the false-alarm we must not
    /// raise — the whole reason a passive `last_packet_age_ms` threshold cannot solve the problem.
    #[test]
    fn idle_but_answered_probe_is_still_live() {
        // last spontaneous packet 45s ago (a normal solo-idle gap), but the probe replied 2s ago.
        assert!(wr(Some(s(30)), Some(s(2)), s(45)),
            "an idle world that ANSWERS the probe is alive — do not false-alarm on app-silence alone");
    }

    /// A probe answered by its own reply is live even with zero spontaneous traffic.
    #[test]
    fn answered_probe_reports_live() {
        assert!(wr(Some(s(30)), Some(s(1)), s(30)));
    }

    /// A probe in flight but not yet overdue must NOT false-alarm — ordinary round-trip latency is
    /// not a wedge. Only crossing the bound flips it.
    #[test]
    fn outstanding_probe_within_the_bound_is_not_yet_a_wedge() {
        // Unanswered (last packet predates the probe → no proof of life since), but 3s < 10s bound.
        assert!(wr(Some(s(3)), None, s(20)),
            "a 3s-old unanswered probe (bound 10s) is still in flight, not a wedge");
        // ...and one whose prior reply predates the newest send is likewise still outstanding.
        assert!(wr(Some(s(3)), Some(s(20)), s(20)),
            "a reply OLDER than the latest probe does not answer it, but 3s < 10s is not yet overdue");
    }

    /// Spontaneous application traffic since the probe was sent proves the world is processing even
    /// if that one probe reply was dropped — a busy zone must never read as wedged. This is the
    /// belt-and-suspenders clause.
    #[test]
    fn spontaneous_traffic_since_the_probe_proves_liveness() {
        // probe sent 15s ago, no probe reply, BUT an app packet arrived 1s ago (world is busy).
        assert!(wr(Some(s(15)), None, s(1)),
            "any app packet since the probe proves liveness — a busy zone is never wedged");
    }

    /// Before the first probe fires (e.g. mid zone-in) AND while packets are still fresh, there is no
    /// probe verdict; defer to the passive clock rather than assert a liveness we have not measured.
    #[test]
    fn no_probe_sent_yet_with_fresh_packets_defers_to_alive() {
        assert!(wr(None, None, s(2)),
            "no probe yet + fresh packets → defer → true (connected/last_packet_age_ms still stand)");
    }

    /// Exactly at the bound counts as overdue (the boundary is closed on the wedge side), so a probe
    /// sitting right at the timeout with no other proof of life reads as wedged.
    #[test]
    fn boundary_at_the_timeout_is_wedged() {
        assert!(!wr(Some(TIMEOUT), None, s(60)),
            "sent_ago == timeout is overdue (not `< timeout`), so it reports wedged");
    }

    // ── #470: the zombie-session honesty fix ────────────────────────────────────────────────────

    /// THE #470 bug, mutation-checked. A failed world-reconnect kills the net thread AND its prober,
    /// so `first_unanswered_probe_sent` is `None` forever while the link goes dead (`connected:false`)
    /// and no packet has arrived for minutes. The pre-#470 `None => true` returned `true` here
    /// UNCONDITIONALLY — a fully dead session that reads alive forever. It MUST now read dead.
    /// Mutation check: revert the fix to `None => true` (or drop the `if !connected` gate) and this
    /// assertion flips to a failure — it cannot pass without the honesty fix.
    #[test]
    fn dead_link_with_no_probe_is_not_responsive() {
        assert!(!world_responsive(false, None, None, s(300), TIMEOUT, STALE),
            "connected:false + stale packets + no outstanding probe is a ZOMBIE, not a live world (#470)");
    }

    /// A dead link is dead even if a probe was once outstanding and even mid-flight — the link gate
    /// precedes every probe branch. (Belt-and-suspenders: the zombie's real state is `None`, but the
    /// gate must not depend on that.)
    #[test]
    fn dead_link_overrides_any_probe_state() {
        assert!(!world_responsive(false, Some(s(1)), Some(s(1)), s(1), TIMEOUT, STALE),
            "connected:false condemns the world regardless of a fresh-looking probe verdict (#470)");
    }

    /// The #343 idle-but-ANSWERED session, in its real no-streak form: `record_probe_reply` clears
    /// `first_unanswered_probe_sent` the instant a genuine reply lands, so an answered idle session has
    /// NO outstanding streak (`None`) even though its last spontaneous packet is 45s stale. A fresh
    /// probe reply is proof of life and must keep it alive — the passive staleness gate must consider
    /// the probe-reply clock, not the spontaneous-packet clock alone, or a healthy idle session reads
    /// as a #470 zombie.
    #[test]
    fn no_streak_but_fresh_probe_reply_is_alive_despite_stale_packets() {
        assert!(world_responsive(true, None, Some(s(2)), s(45), TIMEOUT, STALE),
            "an answered idle session (streak cleared, reply 2s ago) is alive even at 45s app-silence");
    }

    /// The positive companion the fix must NOT regress: a healthy in-session state — link alive,
    /// recent packet, no outstanding probe — still reads alive. This is the ordinary active-play case
    /// (no app-silence → the prober never fires) and it must stay `true`.
    #[test]
    fn healthy_connected_session_with_recent_packet_and_no_probe_is_alive() {
        assert!(world_responsive(true, None, None, s(1), TIMEOUT, STALE),
            "a live link with fresh traffic and no probe outstanding is a healthy session (#470)");
    }

    /// The #343-idle guard for the passive path: a CONNECTED session with no probe outstanding must
    /// stay alive right up to the staleness bound (40s = one full probe cycle + reply window), so an
    /// answered-idle session — whose proof-of-life climbs to nearly a full `PROBE_INTERVAL` between
    /// answered probes — never false-alarms. The bound MUST exceed the resend cadence; below it lies
    /// the regression the reviewer caught. See `gameplay.rs::wedge_timeline_tests` for the end-to-end
    /// cadence proof over a real probe timeline.
    #[test]
    fn connected_no_probe_defers_below_the_passive_bound() {
        assert!(world_responsive(true, None, None, s(30), TIMEOUT, STALE),
            "30s app-silence (one full probe interval) with a live link must still defer to alive (< 40s bound)");
        // ...and a live prober would have re-probed at 30s and, unanswered, moved to the Some/timeout
        // branch by ~40s — so still sitting in the `None` branch past the bound means the prober is
        // gone (#470) → condemn.
        assert!(!world_responsive(true, None, None, STALE, TIMEOUT, STALE),
            "at/after the passive bound with no probe ever, the prober is dead → zombie (#470)");
    }
}

/// #760: `HealthClock` — where a health projection reads "now" when it turns [`NetHealth`]'s
/// stamps into ages. These pin the two properties the fix rests on: the production/`Default` clock
/// is the real wall clock (anything else would freeze every published age at process start — the
/// #343 lie), and a pinned clock yields ages that are a pure function of the stamps, so a fixture
/// built from it cannot drift with machine load.
#[cfg(test)]
mod health_clock_tests {
    use super::{HealthClock, NetHealth};

    // ── `HealthClock` — where a health projection reads "now" (#760) ────────────────────────────

    /// The production clock is the wall clock, and `Default` is the production clock. If this ever
    /// flipped, every age the client publishes would freeze at process start and `connected` would
    /// latch true forever — the exact #343 lie.
    #[test]
    fn the_default_health_clock_is_the_wall_clock() {
        assert!(!HealthClock::default().is_frozen(), "the default clock must be the real wall clock");
        assert!(!HealthClock::WALL.is_frozen());
        assert!(!NetHealth::default().clock.is_frozen(),
            "a NetHealth built the production way must carry the wall clock");
        let before = std::time::Instant::now();
        let a = HealthClock::WALL.now();
        assert!(a >= before, "the wall clock must actually read the wall clock");
    }

    /// A pinned clock reads back the SAME instant every time, so an age against it is a pure
    /// function of the two values and cannot move with machine load. This is the whole of #760.
    #[test]
    fn a_frozen_clock_does_not_advance_and_ages_exactly() {
        let t = std::time::Instant::now();
        let c = HealthClock::frozen_at(t);
        assert!(c.is_frozen());
        assert_eq!(c.now(), t);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(c.now(), t, "a pinned clock must not advance with wall time");
        assert_eq!(c.age_of(t), std::time::Duration::ZERO,
            "a stamp taken at the pin reads exactly zero, however long ago that was");
        assert_eq!(
            c.age_of(t - std::time::Duration::from_secs(4)),
            std::time::Duration::from_secs(4),
            "and an older stamp reads exactly its own age, with no wall-clock drift added");
    }

    /// `ago` inverts `age_of` **on the same clock**: exactly, on a pinned clock; up to the elapsed
    /// gap between the two calls, on the wall clock — where an exact round trip is impossible, which
    /// is why the wall-clock arm below is an inequality and the pinned arm is not. Either way the
    /// call is the right one, so a test never has to choose between two spellings.
    /// The cross-clock half is the one that matters: a stamp taken from the WALL clock and read back
    /// against a PINNED one drifts by the gap between them, which is #760's failure mode one level
    /// down (review finding B1). Asserted here as a strict inequality, not a tolerance window.
    #[test]
    fn ago_is_the_inverse_of_age_of_on_the_same_clock_and_drifts_across_clocks() {
        let pin = std::time::Instant::now();
        let frozen = HealthClock::frozen_at(pin);
        assert_eq!(frozen.age_of(frozen.ago(15)), std::time::Duration::from_secs(15),
            "on a pinned clock, ago(15) must read back as exactly 15s");
        // The wall clock's version of the same law, stated as the inequality that is actually true:
        // `now` advances between the two calls, so the age can only read LONGER than asked, never
        // shorter. (Writing this as `assert_eq!(…, ZERO)` is what a first draft said; it failed at
        // `left: 100ns`, which is this PR's own defect class — a wall-clock-dependent assertion —
        // caught by the suite. Monotonicity gives a bound that needs no tolerance window.)
        assert!(HealthClock::WALL.age_of(HealthClock::WALL.ago(15)) >= std::time::Duration::from_secs(15),
            "on the wall clock, ago(15) must read back as AT LEAST 15s — a monotonic clock cannot \
             make a stamp younger than it was minted");

        // The cross-clock error the guard test exists to ban: stamp from the wall clock AFTER the
        // pin, read back against the pin. The gap is real elapsed time, so the age comes back SHORT.
        std::thread::sleep(std::time::Duration::from_millis(40));
        let wall_stamp = HealthClock::WALL.ago(15);
        assert!(frozen.age_of(wall_stamp) < std::time::Duration::from_secs(15),
            "a wall-clock stamp read against a pinned clock must age SHORT — this is the drift that \
             eats a threshold test's margin (#760/B1)");
    }

    /// A stamp AHEAD of the clock saturates to zero rather than panicking — reachable once the clock
    /// is pinnable, because a fixture may re-stamp a field after freezing.
    #[test]
    fn a_stamp_ahead_of_the_clock_saturates_to_zero() {
        let t = std::time::Instant::now();
        let c = HealthClock::frozen_at(t);
        assert_eq!(c.age_of(t + std::time::Duration::from_secs(9)), std::time::Duration::ZERO);
    }

    /// `NetHealth::frozen_at` stamps the three liveness clocks at exactly the ages asked for,
    /// measured against its own pin — so a fixture's `(0,0,0)` is a permanently live session and its
    /// `(20,0,20)` is a permanently disconnected one, with no margin for load to eat.
    #[test]
    fn net_health_frozen_at_yields_exact_ages() {
        let now = std::time::Instant::now();
        let h = NetHealth::frozen_at(now, 20, 6, 41);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(h.clock.age_of(h.last_datagram), std::time::Duration::from_secs(20));
        assert_eq!(h.clock.age_of(h.last_tick), std::time::Duration::from_secs(6));
        assert_eq!(h.clock.age_of(h.last_packet), std::time::Duration::from_secs(41));
        let live = NetHealth::frozen_at(std::time::Instant::now(), 0, 0, 0);
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert_eq!(live.clock.age_of(live.last_tick), std::time::Duration::ZERO,
            "a (0,0,0) fixture stays exactly fresh no matter how long the test runs");
    }
}

/// #656: the io-driver-starvation alert, tested as a pure function (`send_starved`) plus the
/// `NetHealth::record_send_pressure` state machine that feeds it. These pin the fire/clear
/// boundary the issue asked for: a signal that is TRUE while starvation is actually happening and
/// CLEARS once it stops — never a lifetime count that only grows, and never one that never fires.
#[cfg(test)]
mod send_starved_tests {
    use super::{send_starved, NetHealth, SEND_PRESSURE_BURST_GAP_SECS, SEND_PRESSURE_FIRE_THRESHOLD};
    use std::time::{Duration, Instant};

    fn s(secs: u64) -> Duration { Duration::from_secs(secs) }

    // ── `send_starved` — the pure fire/clear predicate ──────────────────────────────────────────

    /// Never having seen a pressure event at all must read as healthy — there is nothing to alert
    /// on. This is the process-start / never-under-pressure default.
    #[test]
    fn no_pressure_event_ever_is_not_starved() {
        assert!(!send_starved(0, None), "no pressure event ever recorded must read as healthy");
        // Even a nonsensical nonzero streak with no timestamp must not fire — the age is what gates
        // recency, and there is none to measure.
        assert!(!send_starved(999, None));
    }

    /// THE fire case: a burst at/above the threshold, freshly stamped, must fire.
    #[test]
    fn a_fresh_burst_at_the_threshold_fires() {
        assert!(send_starved(SEND_PRESSURE_FIRE_THRESHOLD, Some(s(0))),
            "a burst that just reached the fire threshold, happening right now, must alert");
    }

    /// Below the threshold must NOT fire, no matter how fresh — this is the #603/#610 single-stray-
    /// WouldBlock-after-connect case, which is documented normal behavior, not CPU pressure.
    #[test]
    fn below_the_threshold_never_fires_even_when_fresh() {
        assert!(!send_starved(SEND_PRESSURE_FIRE_THRESHOLD - 1, Some(s(0))),
            "one event short of the threshold must not alert even at zero age");
        assert!(!send_starved(1, Some(s(0))),
            "a single isolated pressure event must never fire — that is the documented harmless case");
    }

    /// THE clear case, mutation-checked: a burst that once reached the threshold, but whose most
    /// recent event has aged past the burst-gap window, must have CLEARED — regardless of how large
    /// the historical streak is. This is the field #656 exists to add: the previous counters
    /// (`send_wouldblock_rescued`/`send_deferred`) can only grow and could never represent this.
    #[test]
    fn a_stale_burst_clears_regardless_of_streak_size() {
        assert!(!send_starved(200, Some(s(SEND_PRESSURE_BURST_GAP_SECS + 1))),
            "a burst of 200 events must still read as CLEARED once its most recent event is older \
             than the burst-gap window — the alert must not latch on a historical peak");
    }

    /// Boundary: exactly at the burst-gap age still counts as "still going" (closed on the fire
    /// side); one tick past it has cleared.
    #[test]
    fn burst_gap_boundary_is_closed_on_the_firing_side() {
        assert!(send_starved(SEND_PRESSURE_FIRE_THRESHOLD, Some(s(SEND_PRESSURE_BURST_GAP_SECS))),
            "age == the burst gap exactly must still count as within the burst");
        assert!(!send_starved(SEND_PRESSURE_FIRE_THRESHOLD, Some(s(SEND_PRESSURE_BURST_GAP_SECS) + Duration::from_millis(1))),
            "one instant past the burst gap must have cleared");
    }

    // ── `NetHealth::record_send_pressure` — the streak state machine ───────────────────────────

    /// A run of events arriving well within the burst gap of each other must accumulate the streak,
    /// not reset it — this is what lets a genuine ~10ms-cadence burst actually cross the fire
    /// threshold instead of perpetually reading `streak == 1`.
    #[test]
    fn consecutive_events_within_the_gap_accumulate_the_streak() {
        let mut h = NetHealth::default();
        let base = Instant::now();
        for i in 0..SEND_PRESSURE_FIRE_THRESHOLD {
            h.record_send_pressure(base + Duration::from_millis(10 * i));
        }
        assert_eq!(h.send_pressure_streak, SEND_PRESSURE_FIRE_THRESHOLD,
            "events 10ms apart (well under the multi-second burst gap) must all join one streak");
        assert_eq!(h.last_send_pressure_at, Some(base + Duration::from_millis(10 * (SEND_PRESSURE_FIRE_THRESHOLD - 1))));
    }

    /// A gap longer than `SEND_PRESSURE_BURST_GAP_SECS` between two events must start a FRESH streak
    /// at 1, not add to the old one — mutation check: an unconditional `+= 1` here would let an old,
    /// long-finished burst's count silently carry forward into a new, unrelated single event and
    /// falsely cross the fire threshold on its own over enough process lifetime.
    #[test]
    fn a_gap_past_the_burst_window_resets_the_streak_to_one() {
        let mut h = NetHealth::default();
        let base = Instant::now();
        for i in 0..3 {
            h.record_send_pressure(base + Duration::from_millis(10 * i));
        }
        assert_eq!(h.send_pressure_streak, 3, "fixture check: the first burst accumulated");

        // The gap must be measured from the LAST recorded event (base + 20ms), not from `base` —
        // otherwise this fixture doesn't actually exercise a stale gap at all.
        let last_event = base + Duration::from_millis(20);
        let after_gap = last_event + Duration::from_secs(SEND_PRESSURE_BURST_GAP_SECS) + Duration::from_millis(1);
        h.record_send_pressure(after_gap);
        assert_eq!(h.send_pressure_streak, 1,
            "an event arriving after the burst gap has elapsed must start a NEW streak, not extend \
             the old one — otherwise unrelated events far apart in time could sum past the fire \
             threshold and falsely alert");
        assert_eq!(h.last_send_pressure_at, Some(after_gap));
    }
}

/// #643 review round 2 — the roster-publisher invariant.
#[cfg(test)]
mod world_roster_tests_643 {
    use super::WorldSlots;
    use eqoxide_core::game_state::{make_entity, Gait, Pose};

    /// `publish_entities` writes all three maps or none — the guarantee
    /// `/v1/observe/entities?labeled=1` makes to agents. MUTATION CHECK: delete any one of the
    /// three `insert`s (or any one `clear`) in `publish_entities` and this goes RED.
    #[test]
    fn publish_entities_writes_all_three_maps_with_identical_keys() {
        let world = WorldSlots::default();

        let mut sitter = make_entity(1, "a_sitter", 1.0, 2.0, 3.0, true);
        sitter.pose = Pose::Sitting;
        sitter.gait = Some(Gait::from_wire_10bit(12));
        let mut walker = make_entity(2, "a_walker", 4.0, 5.0, 6.0, true);
        walker.gait = Some(Gait::from_wire_10bit(1012)); // backing up: -12
        let entities: std::collections::HashMap<u32, _> =
            [(1u32, sitter), (2u32, walker)].into_iter().collect();

        assert_eq!(world.publish_entities(&entities), 2);

        let positions = world.entity_positions();
        let ids       = world.entity_ids();
        let poses     = world.entity_poses();

        fn sorted<V>(m: &std::collections::HashMap<String, V>) -> Vec<String> {
            let mut v: Vec<String> = m.keys().cloned().collect();
            v.sort();
            v
        }
        assert_eq!(sorted(&positions), sorted(&ids),
            "positions and ids must have identical key sets");
        assert_eq!(sorted(&positions), sorted(&poses),
            "positions and poses must have identical key sets — an agent indexes `poses` by a name \
             it read from `entities`");

        assert_eq!(positions["a_sitter"], (1.0, 2.0, 3.0));
        assert_eq!(ids["a_sitter"], 1);
        assert_eq!(poses["a_sitter"].pose, "sitting");
        assert_eq!(poses["a_sitter"].gait, Some(12));
        assert_eq!(poses["a_walker"].pose, "standing");
        assert_eq!(poses["a_walker"].gait, Some(-12), "a backing-up mob's gait stays negative");
    }

    /// A second publish must FULL-REPLACE, not merge: an entity from the previous zone (or the
    /// previous login attempt) must not survive in any of the three maps.
    #[test]
    fn publish_entities_full_replaces_so_no_stale_entity_survives() {
        let world = WorldSlots::default();
        let first: std::collections::HashMap<u32, _> =
            [(1u32, make_entity(1, "old_zone_mob", 0.0, 0.0, 0.0, true))].into_iter().collect();
        world.publish_entities(&first);
        let second: std::collections::HashMap<u32, _> =
            [(2u32, make_entity(2, "new_zone_mob", 0.0, 0.0, 0.0, true))].into_iter().collect();
        world.publish_entities(&second);

        assert!(!world.entity_positions().contains_key("old_zone_mob"));
        assert!(!world.entity_ids().contains_key("old_zone_mob"));
        assert!(!world.entity_poses().contains_key("old_zone_mob"),
            "a stale pose is worse than a stale position — it is a confident claim about a body \
             state that no longer exists");
    }

}
