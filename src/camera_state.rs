//! Orbit/follow camera state: azimuth/elevation/radius with smoothing toward the player, mouse-drag
//! and scroll input, and the `CameraCmd`/`CameraSnapshot` types shared with the HTTP `/camera`
//! endpoint (the agent sets a command; the render loop applies it and publishes a snapshot back).
//!
//! `CameraMode`/`CameraCmd`/`CameraSnapshot` are pure inter-thread contract data — they moved DOWN
//! into `eqoxide-ipc` (#544 Step 2c) so that crate's `CameraSlots` no longer up-references
//! `camera_state`. The BEHAVIOR (`CameraState` and its update/snapshot logic) stays here and `use`s
//! those types. Re-exported so every existing `crate::camera_state::{CameraMode,CameraCmd,
//! CameraSnapshot}` path across the tree keeps resolving unchanged.
pub use eqoxide_ipc::{desired_azimuth, CameraCmd, CameraMode, CameraSnapshot};

pub const ELEVATION_MIN: f32 = 0.08727; // 5°
pub const ELEVATION_MAX: f32 = 1.39626; // 80°
pub const RADIUS_MIN:    f32 = 20.0;
pub const RADIUS_MAX:    f32 = 500.0;
const DESIRED_ELEVATION: f32 = 0.34907; // 20° (default tilt; restored only by F9/R/reset)
const DESIRED_RADIUS:    f32 = 80.0;
/// How high above the player's feet the camera looks. Humanoids are ~20 EQ units
/// tall, so 5 units targets roughly the mid-torso.
const LOOK_TARGET_Z:     f32 = 5.0;
const FOLLOW_RATE:       f32 = 5.0;

/// Interpolate angle `from` toward `to` by `alpha`, taking the short arc (handles 0/2π wrap).
pub fn lerp_angle(from: f32, to: f32, alpha: f32) -> f32 {
    use std::f32::consts::{PI, TAU};
    let mut diff = (to - from).rem_euclid(TAU);
    if diff > PI { diff -= TAU; }
    from + diff * alpha
}

/// Linear interpolation of a 3-element array.
pub fn lerp3(from: [f32; 3], to: [f32; 3], alpha: f32) -> [f32; 3] {
    [
        from[0] + (to[0] - from[0]) * alpha,
        from[1] + (to[1] - from[1]) * alpha,
        from[2] + (to[2] - from[2]) * alpha,
    ]
}

/// Compute world-space eye position from spherical parameters.
/// az=0 → +X, counterclockwise. el=0 → horizon, el=π/2 → zenith. Z-up.
pub fn compute_eye(azimuth: f32, elevation: f32, radius: f32, focus: [f32; 3]) -> [f32; 3] {
    [
        focus[0] + radius * elevation.cos() * azimuth.cos(),
        focus[1] + radius * elevation.cos() * azimuth.sin(),
        focus[2] + radius * elevation.sin(),
    ]
}

/// `(eye, look_target)` for a spherical camera around `focus` — the same pair [`CameraState::tick`]
/// returns each frame for the live camera. Extracted as a free function (#422) so a one-off capture
/// (`GET /v1/observe/frame`'s per-request camera override) can compute the identical eye/look-target
/// pair for an ARBITRARY (azimuth, elevation, radius) without going through `CameraState` at all —
/// the override never touches the live camera's fields, only this pure math.
pub fn eye_and_look(azimuth: f32, elevation: f32, radius: f32, focus: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let eye  = compute_eye(azimuth, elevation, radius, focus);
    let look = [focus[0], focus[1], focus[2] + LOOK_TARGET_Z];
    (eye, look)
}

/// Inverse of [`desired_azimuth`]: the heading (EQ degrees, CCW) a character must face
/// to be looking in the camera's horizontal direction. Used by mouse-look "drive" mode,
/// where the character's heading is slaved to the camera while LMB + a move key are held.
pub fn heading_deg_from_azimuth(azimuth: f32) -> f32 {
    (azimuth + std::f32::consts::FRAC_PI_2).to_degrees().rem_euclid(360.0)
}

/// The eye actually usable to render a frame, together with whether it differs from what was
/// asked for. Returned by [`resolve_camera_eye`] — see that function's doc comment for why this
/// exists (#852).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedEye {
    /// The eye position to render from THIS frame.
    pub eye:           [f32; 3],
    /// True iff pull-in moved `eye` away from the desired orbit position.
    pub occluded:      bool,
    /// True iff, even after the pull-in loop's iteration budget, the focus→eye segment is still
    /// not clear of collision (a degenerate case: e.g. the camera's minimum radius still doesn't
    /// fit the space). A caller that renders `eye` anyway in this case is drawing a frame that
    /// still has geometry between the lens and the target — this flag is how it says so instead
    /// of rendering it silently.
    pub still_blocked: bool,
}

/// Pull `desired_eye` in along the segment toward `focus` until the segment is clear of
/// `collision`, or the iteration budget is spent. Mirrors a standard third-person camera: cast
/// from the look target toward the desired eye each frame, and stop short of the first hit.
///
/// #852: **this is the single place camera collision is resolved.** Both the on-screen render
/// and the published `/v1/observe/debug` snapshot must be built from this same return value —
/// never from a value either one recomputes independently — so "what was rendered" and "what was
/// reported" cannot drift apart the way they did before this fix (the old call site in `app.rs`
/// mutated a local `cam_eye` for rendering but published `CameraState::snapshot()`'s untouched
/// `radius`, so an agent reading `/v1/observe/debug` got the pre-collision eye 88% of the time a
/// pull-in fired — see #852).
///
/// A single pass can fail in multi-story buildings where the eye lands between two floor slabs,
/// hence the small iteration budget rather than one shot.
pub fn resolve_camera_eye(
    collision: Option<&crate::nav::collision::Collision>,
    focus: [f32; 3],
    desired_eye: [f32; 3],
) -> ResolvedEye {
    let mut eye = desired_eye;
    let mut occluded = false;
    let mut still_blocked = false;
    if let Some(col) = collision {
        for _ in 0..5 {
            match col.nearest_hit_t(focus, eye) {
                Some(t) => {
                    still_blocked = true;
                    let frac = (t * 0.85).clamp(0.05, 1.0);
                    let new_eye = lerp3(focus, eye, frac);
                    if new_eye == eye { break; }
                    eye = new_eye;
                    occluded = true;
                }
                None => {
                    still_blocked = false;
                    break;
                }
            }
        }
    }
    ResolvedEye { eye, occluded, still_blocked }
}

// `CameraMode`, `CameraCmd`, and `CameraSnapshot` moved to `eqoxide-ipc` (#544 Step 2c) — re-exported
// at the top of this module.

pub struct CameraState {
    pub azimuth:         f32,
    pub elevation:       f32,
    pub radius:          f32,
    pub focus:           [f32; 3],
    pub mode:            CameraMode,
}

impl CameraState {
    /// Initialise in AutoFollow mode, pointed at `player_pos` from behind `heading_deg`.
    pub fn new(player_pos: [f32; 3], heading_deg: f32) -> Self {
        Self {
            azimuth:         desired_azimuth(heading_deg),
            elevation:       DESIRED_ELEVATION,
            radius:          DESIRED_RADIUS,
            focus:           player_pos,
            mode:            CameraMode::AutoFollow,
        }
    }

    /// Advance camera state by `dt` seconds. Returns `(eye_pos, look_target)`.
    pub fn tick(&mut self, dt: f32, player_pos: [f32; 3], heading_deg: f32) -> ([f32; 3], [f32; 3]) {
        let des_az = desired_azimuth(heading_deg);

        // The focus ALWAYS tracks the player's position, in either mode — the camera stays
        // centered on the character while it walks (a ManualOrbit camera must not be left behind
        // during /goto nav). Orbit mode only governs the view *angles*, not the look-at point.
        let alpha  = 1.0 - (-FOLLOW_RATE * dt).exp();
        self.focus = lerp3(self.focus, player_pos, alpha);
        // AutoFollow additionally swings the camera behind the heading. ManualOrbit preserves the
        // user's chosen azimuth/elevation/radius (only F9/R/HTTP reset restores the defaults).
        if self.mode == CameraMode::AutoFollow {
            self.azimuth = des_az;
        }

        eye_and_look(self.azimuth, self.elevation, self.radius, self.focus)
    }

    /// Adjust azimuth and elevation from mouse drag (radians/pixel × pixel-delta).
    /// DEBUG: elevation only guarded against the exact poles (gimbal flip), otherwise free.
    pub fn apply_orbit_delta(&mut self, daz: f32, del: f32) {
        const POLE: f32 = std::f32::consts::FRAC_PI_2 - 0.001;
        self.azimuth   = (self.azimuth + daz).rem_euclid(std::f32::consts::TAU);
        self.elevation = (self.elevation - del).clamp(-POLE, POLE);
        self.mode      = CameraMode::ManualOrbit;
    }

    /// Zoom by `factor` scroll lines (positive = zoom in).
    /// DEBUG: radius unconstrained (any distance).
    pub fn apply_zoom(&mut self, factor: f32) {
        self.radius = (self.radius * (1.0 - factor)).max(0.01);
        self.mode   = CameraMode::ManualOrbit;
    }

    /// Full reset (R/F9 key or HTTP reset): re-enter AutoFollow AND restore the default
    /// tilt and zoom. This is the ONLY path that snaps elevation/radius back to defaults.
    pub fn reset_to_follow(&mut self) {
        self.mode      = CameraMode::AutoFollow;
        self.elevation = DESIRED_ELEVATION;
        self.radius    = DESIRED_RADIUS;
    }

    /// Rotate the camera rigidly with the character's heading by `d_az` radians, preserving
    /// its current relative offset (it does NOT snap behind). Used when rotating with A/D
    /// (no mouse button): the character and camera turn together. Switches to ManualOrbit so
    /// the AutoFollow tick won't re-derive azimuth from heading and undo this.
    pub fn rotate_with_heading(&mut self, d_az: f32) {
        self.azimuth = (self.azimuth + d_az).rem_euclid(std::f32::consts::TAU);
        self.mode    = CameraMode::ManualOrbit;
    }

    /// Apply an HTTP command to the camera state.
    pub fn apply_cmd(&mut self, cmd: CameraCmd) {
        match cmd {
            CameraCmd::Set { azimuth, elevation, radius, focus } => {
                // DEBUG: unconstrained — any direction, distance, and focus point.
                if let Some(az) = azimuth   { self.azimuth   = az; }
                if let Some(el) = elevation { self.elevation = el; }
                if let Some(r)  = radius    { self.radius    = r; }
                if let Some(f)  = focus     { self.focus     = f; }
                self.mode       = CameraMode::ManualOrbit;
            }
            CameraCmd::Reset => self.reset_to_follow(),
        }
    }

    /// Snapshot describing the frame `drawn` proves was encoded — **not** "the current state".
    ///
    /// `resolved` is the [`ResolvedEye`] the SAME `resolve_camera_eye` call that produced the
    /// eye/look-target passed to that `render_frame` returned — this method just carries
    /// it through into the published struct, it does not recompute an eye from
    /// `azimuth`/`elevation`/`radius`/`focus` itself (#852: that recomputation is exactly the bug
    /// that fixed — it would silently reproduce the pre-collision eye).
    ///
    /// `drawn` is #867's structural half. `App::render_frame` can `return` before the draw on three
    /// `wgpu::SurfaceError` arms, and a publish that only *textually* follows the render call is
    /// still reachable from before it after any ordinary refactor (extract-method, an `Arc::clone`
    /// alias). Taking an [`eqoxide_renderer::DrawnFrame`] by value instead means a pre-draw caller
    /// has no argument to pass and does not compile. See that type's doc for the exact scope: it
    /// proves a `render_frame` call returned, not that the frame was submitted or presented.
    ///
    /// For the pre-first-frame seed, use [`CameraState::undrawn_snapshot`], which is honest about
    /// having no frame rather than borrowing one.
    pub fn snapshot(&self, resolved: ResolvedEye, drawn: eqoxide_renderer::DrawnFrame) -> CameraSnapshot {
        CameraSnapshot {
            drawn_frame:   Some(drawn.index()),
            drawn_at:      Some(std::time::Instant::now()),
            ..self.undrawn_snapshot(resolved)
        }
    }

    /// The startup seed (#867): a snapshot with `drawn_frame`/`drawn_at` = `None`, i.e. **no frame
    /// has been drawn yet**.
    ///
    /// `main.rs` publishes one of these before `EventLoop::new()`, so `/v1/camera` and
    /// `/v1/observe/debug` serve a camera block through GPU init, `resumed()`, and the first zone
    /// load. Before #867 that seed was indistinguishable from a rendered snapshot;
    /// `drawn_frame: None` makes the pre-first-frame state representable, which is why the docs
    /// can say what the fields mean without a "never" that startup falsifies.
    pub fn undrawn_snapshot(&self, resolved: ResolvedEye) -> CameraSnapshot {
        CameraSnapshot {
            mode:          self.mode,
            azimuth:       self.azimuth,
            elevation:     self.elevation,
            radius:        self.radius,
            focus:         self.focus,
            eye:           resolved.eye,
            occluded:      resolved.occluded,
            still_blocked: resolved.still_blocked,
            drawn_frame:   None,
            drawn_at:      None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::{PI, TAU};

    #[test]
    fn lerp_angle_takes_short_arc_across_zero() {
        let a = 350.0_f32.to_radians();
        let b = 10.0_f32.to_radians();
        let result = lerp_angle(a, b, 0.5);
        let norm = result.rem_euclid(TAU);
        assert!(
            norm > 340.0_f32.to_radians() || norm < 20.0_f32.to_radians(),
            "expected near 0° but got {:.1}°", norm.to_degrees()
        );
    }

    #[test]
    fn lerp_angle_no_wrap_needed_is_plain_lerp() {
        let a = 0.0_f32;
        let b = PI;
        let result = lerp_angle(a, b, 0.5);
        assert!((result - PI / 2.0).abs() < 1e-5, "got {result}");
    }

    #[test]
    fn compute_eye_south_of_focus_at_az_minus_half_pi() {
        let eye = compute_eye(-PI / 2.0, 0.0, 10.0, [0.0, 0.0, 0.0]);
        assert!((eye[0]).abs() < 1e-4, "x={}", eye[0]);
        assert!((eye[1] + 10.0).abs() < 1e-4, "y={}", eye[1]);
        assert!((eye[2]).abs() < 1e-4, "z={}", eye[2]);
    }

    #[test]
    fn compute_eye_respects_focus_offset() {
        let focus = [5.0_f32, 0.0, 0.0];
        let eye = compute_eye(-PI / 2.0, 0.0, 10.0, focus);
        assert!((eye[0] - 5.0).abs() < 1e-4, "x={}", eye[0]);
        assert!((eye[1] + 10.0).abs() < 1e-4, "y={}", eye[1]);
    }

    #[test]
    fn eye_and_look_matches_tick_for_the_same_spherical_params() {
        // #422: the per-capture override path calls `eye_and_look` directly (bypassing
        // `CameraState`); pin that it reproduces exactly what `tick()` would have returned for the
        // same (azimuth, elevation, radius, focus) — the refactor that extracted it must not have
        // changed the live camera's math.
        let mut cam = CameraState::new([1.0, 2.0, 3.0], 40.0);
        cam.apply_orbit_delta(0.3, 0.1);
        cam.radius = 123.0;
        let (tick_eye, tick_look) = cam.tick(0.0, [1.0, 2.0, 3.0], 40.0);
        let (fn_eye, fn_look) = eye_and_look(cam.azimuth, cam.elevation, cam.radius, cam.focus);
        assert_eq!(tick_eye, fn_eye);
        assert_eq!(tick_look, fn_look);
    }

    #[test]
    fn compute_eye_elevation_lifts_z() {
        let eye = compute_eye(0.0, PI / 2.0, 10.0, [0.0, 0.0, 0.0]);
        assert!((eye[2] - 10.0).abs() < 1e-4, "z={}", eye[2]);
    }

    #[test]
    fn desired_azimuth_heading_north_gives_south_camera() {
        let az = desired_azimuth(0.0);
        let eye_dir_y = az.sin();
        assert!(eye_dir_y < -0.9, "camera should be south of focus, got sin(az)={eye_dir_y}");
    }

    #[test]
    fn desired_azimuth_heading_west_gives_east_camera() {
        // CCW heading 90 = west → camera behind = east
        let az = desired_azimuth(90.0);
        let eye_dir_x = az.cos();
        assert!(eye_dir_x > 0.9, "camera should be east of focus, got cos(az)={eye_dir_x}");
    }

    #[test]
    fn desired_azimuth_heading_east_gives_west_camera() {
        // CCW heading 270 = east → camera behind = west
        let az = desired_azimuth(270.0);
        let eye_dir_x = az.cos();
        assert!(eye_dir_x < -0.9, "camera should be west of focus, got cos(az)={eye_dir_x}");
    }

    #[test]
    fn tick_autofollow_snaps_azimuth_instantly() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.azimuth = 0.0;
        cam.tick(0.016, [0.0, 0.0, 0.0], 0.0);
        let expected = desired_azimuth(0.0);
        assert!((cam.azimuth - expected).abs() < 1e-5);
    }

    #[test]
    fn tick_manual_orbit_stays_manual_when_idle_and_moving() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_orbit_delta(0.1, 0.0);
        cam.tick(0.016, [0.0, 0.0, 0.0], 0.0);
        assert_eq!(cam.mode, CameraMode::ManualOrbit);
    }

    #[test]
    fn tick_manual_orbit_does_not_recover_when_player_still() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_orbit_delta(0.1, 0.0);
        let pos = [0.0_f32, 0.0, 0.0];
        cam.tick(0.016, pos, 0.0);
        assert_eq!(cam.mode, CameraMode::ManualOrbit);
    }

    #[test]
    fn reset_to_follow_returns_to_autofollow() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_orbit_delta(0.5, 0.2);
        assert_eq!(cam.mode, CameraMode::ManualOrbit);
        cam.reset_to_follow();
        assert_eq!(cam.mode, CameraMode::AutoFollow);
    }

    #[test]
    fn autofollow_tick_preserves_manual_tilt_and_zoom() {
        // After zooming/tilting, movement must NOT snap tilt/zoom back.
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.elevation = 0.9;
        cam.radius    = 150.0;
        cam.mode      = CameraMode::AutoFollow;
        cam.tick(0.016, [10.0, 10.0, 0.0], 90.0); // player moves + turns
        assert!((cam.elevation - 0.9).abs() < 1e-6, "tilt snapped back: {}", cam.elevation);
        assert!((cam.radius - 150.0).abs() < 1e-6, "zoom snapped back: {}", cam.radius);
    }

    #[test]
    fn reset_to_follow_restores_default_tilt_and_zoom() {
        // F9/R is the ONLY path that restores defaults.
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.elevation = 0.9;
        cam.radius    = 150.0;
        cam.reset_to_follow();
        assert!((cam.elevation - DESIRED_ELEVATION).abs() < 1e-6);
        assert!((cam.radius - DESIRED_RADIUS).abs() < 1e-6);
    }

    #[test]
    fn rotate_with_heading_preserves_relative_offset_and_tiltzoom() {
        // A/D rotation: camera azimuth turns by the same delta as the heading, keeping its
        // relative offset, and tilt/zoom are untouched.
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_orbit_delta(0.3, 0.2); // user orbited to some offset
        let (az0, el, r) = (cam.azimuth, cam.elevation, cam.radius);
        let d = 0.25_f32;
        cam.rotate_with_heading(d);
        assert!((cam.azimuth - (az0 + d).rem_euclid(std::f32::consts::TAU)).abs() < 1e-6);
        assert_eq!(cam.mode, CameraMode::ManualOrbit);
        assert!((cam.elevation - el).abs() < 1e-6);
        assert!((cam.radius - r).abs() < 1e-6);
    }

    #[test]
    fn heading_azimuth_round_trips() {
        for h in [0.0_f32, 30.0, 90.0, 200.0, 359.0] {
            let az = desired_azimuth(h);
            let back = heading_deg_from_azimuth(az);
            let diff = (back - h).rem_euclid(360.0);
            assert!(diff < 1e-3 || diff > 360.0 - 1e-3, "h={h} -> {back}");
        }
    }

    #[test]
    fn default_elevation_is_twenty_degrees() {
        assert!((DESIRED_ELEVATION - 20.0_f32.to_radians()).abs() < 1e-4);
    }

    #[test]
    fn apply_cmd_set_partial_only_updates_supplied_fields() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        let original_az = cam.azimuth;
        cam.apply_cmd(CameraCmd::Set {
            azimuth: None, elevation: Some(1.2), radius: None, focus: None,
        });
        assert!((cam.azimuth - original_az).abs() < 1e-6, "azimuth should be unchanged");
        assert!((cam.elevation - 1.2).abs() < 1e-6);
        assert_eq!(cam.mode, CameraMode::ManualOrbit);
    }

    #[test]
    fn apply_cmd_set_elevation_is_unconstrained() {
        // DEBUG: /camera Set is intentionally unclamped so any angle is reachable.
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_cmd(CameraCmd::Set {
            azimuth: None, elevation: Some(10.0), radius: None, focus: None,
        });
        assert_eq!(cam.elevation, 10.0, "Set should apply elevation verbatim (no clamp)");
    }

    #[test]
    fn apply_cmd_reset_returns_to_autofollow() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_orbit_delta(0.5, 0.2);
        assert_eq!(cam.mode, CameraMode::ManualOrbit);
        cam.apply_cmd(CameraCmd::Reset);
        assert_eq!(cam.mode, CameraMode::AutoFollow);
    }

    // ── #852: the published eye must be the rendered eye ─────────────────────────────────────

    #[test]
    fn resolve_camera_eye_is_a_noop_with_no_collision() {
        let desired = [12.0, -34.0, 56.0];
        let resolved = resolve_camera_eye(None, [0.0, 0.0, 0.0], desired);
        assert_eq!(resolved.eye, desired);
        assert!(!resolved.occluded);
        assert!(!resolved.still_blocked);
    }

    /// A wall standing directly on the segment between `focus` and `desired_eye`, perpendicular to
    /// the east axis at `wall_x` — mirrors `collision::tests::parallel_wall`'s "GLB axes -> world:
    /// east = p[2], north = p[0], height = p[1]" convention, just built here instead of imported
    /// (this crate cannot see `eqoxide-nav`'s `#[cfg(test)]` helpers).
    fn wall_at_east(wall_x: f32) -> crate::nav::collision::Collision {
        use crate::assets::{MeshData, RenderMode, ZoneAssets};
        use crate::nav::collision::Collision;
        let wall = MeshData {
            positions: vec![
                [-500.0, -500.0, wall_x], [500.0, -500.0, wall_x],
                [500.0,   500.0, wall_x], [-500.0, 500.0, wall_x],
            ],
            normals: vec![[0.0, 0.0, -1.0]; 4], uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
            center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
        };
        Collision::build(&ZoneAssets { terrain: vec![wall], objects: vec![], textures: vec![] }, 8.0)
    }

    #[test]
    fn resolve_camera_eye_pulls_in_behind_a_blocking_wall() {
        // Player-side reproduction of the #852 measured case: a wall sits between the focus and
        // the desired (unoccluded) eye. The resolved eye must land on the FOCUS side of the wall,
        // not beyond it.
        let col = wall_at_east(10.0);
        let focus = [0.0_f32, 0.0, 0.0];
        let desired_eye = [80.0_f32, 0.0, 0.0];
        let resolved = resolve_camera_eye(Some(&col), focus, desired_eye);
        assert!(resolved.eye[0] < 10.0,
            "resolved eye x={} is beyond the wall at x=10 — this is the #852 bug", resolved.eye[0]);
        assert!(resolved.occluded, "pulling the eye in must be reported as occluded");
        assert!(!resolved.still_blocked, "the resolved eye landed clear of the wall");
        assert_ne!(resolved.eye, desired_eye);
    }

    /// **THE property test (#852).** For a sweep of wall placements and desired-eye distances, the
    /// resolved eye must never sit on the far side of a wall that stands between `focus` and the
    /// desired eye — checked against an INDEPENDENT geometric fact (the wall's fixed x-coordinate),
    /// not by re-deriving the expected answer from `resolve_camera_eye` itself. A premise counter
    /// guards against the sweep vacuously containing no blocked case.
    ///
    /// Mutation-checked by execution (#852): reverting the fix — i.e. going back to the original
    /// `app.rs` shape where the pull-in loop mutates a local eye that never reaches the published
    /// snapshot — cannot be expressed as a mutation of `resolve_camera_eye` alone (that was the
    /// point: the old bug was in what the CALLER did with the result, not in this function). The
    /// mutation that matters lives at the call site and is guarded by
    /// `snapshot_carries_the_resolved_eye_verbatim_not_a_recomputed_one` below instead.
    #[test]
    fn resolve_camera_eye_never_reports_an_eye_across_a_blocking_wall() {
        let focus = [0.0_f32, 0.0, 0.0];
        let mut blocked_cases = 0;
        // Deliberately none of these equal an `eye_dist` value below — an eye sitting exactly ON
        // the wall plane is a separate degenerate case, not what this sweep is checking.
        for wall_x in [4.0_f32, 11.0, 21.0, 39.0, 49.0, 79.0] {
            let col = wall_at_east(wall_x);
            for eye_dist in [10.0_f32, 40.0, 80.0, 150.0, 500.0] {
                let desired_eye = [eye_dist, 0.0, 0.0];
                let resolved = resolve_camera_eye(Some(&col), focus, desired_eye);
                if wall_x < eye_dist {
                    blocked_cases += 1;
                    assert!(resolved.eye[0] < wall_x,
                        "wall_x={wall_x} eye_dist={eye_dist}: resolved eye x={} crossed the wall",
                        resolved.eye[0]);
                    assert!(resolved.occluded,
                        "wall_x={wall_x} eye_dist={eye_dist}: a pulled-in eye must be reported occluded");
                } else {
                    assert_eq!(resolved.eye, desired_eye,
                        "wall_x={wall_x} eye_dist={eye_dist}: wall is not between focus and the \
                         desired eye, so the eye must be unchanged");
                    assert!(!resolved.occluded);
                }
            }
        }
        assert!(blocked_cases > 0, "premise: the sweep must contain at least one blocked case");
    }

    #[test]
    fn snapshot_carries_the_resolved_eye_verbatim_not_a_recomputed_one() {
        // #852 mutation guard: `snapshot` must publish EXACTLY the `ResolvedEye` it is handed, not
        // an eye it derives itself from `azimuth`/`elevation`/`radius`/`focus` — that recomputation
        // is exactly the bug this fix removes (`snapshot()` used to take no eye at all and always
        // reported the pre-collision position). If `snapshot` were reverted to recompute the eye
        // internally, this test goes RED: the arbitrary `resolved.eye` below is deliberately NOT
        // what `compute_eye` would derive from this camera's own azimuth/elevation/radius/focus.
        let mut cam = CameraState::new([1.0, 2.0, 3.0], 40.0);
        cam.radius = 80.0;
        let resolved = ResolvedEye { eye: [999.0, -42.0, 7.5], occluded: true, still_blocked: true };
        let snap = cam.snapshot(resolved, eqoxide_renderer::DrawnFrame::for_test(11));
        assert_eq!(snap.eye, [999.0, -42.0, 7.5]);
        assert!(snap.occluded);
        assert!(snap.still_blocked);
        let derived = compute_eye(cam.azimuth, cam.elevation, cam.radius, cam.focus);
        assert_ne!(derived, snap.eye,
            "test fixture bug: the arbitrary resolved eye must differ from what compute_eye would \
             derive, or this test cannot tell the two code paths apart");
    }

    /// #867 — a snapshot must say WHICH frame it describes, and the two constructors must disagree
    /// about whether there was one. Without this the caller cannot distinguish a snapshot published
    /// this tick from one frozen since the window was minimised: every other field is identical in
    /// both cases, and the nearby `snapshot_age_ms` on `/v1/observe/debug` is the NETWORK clock, so
    /// it reads fresh throughout a rendering stall.
    ///
    /// The ordering half of #867 is not asserted here — it is enforced by the type system: the
    /// `DrawnFrame` argument below can only be produced by `EqRenderer::render_frame` (its field is
    /// private, its `for_test` constructor is dev-only), so a publisher sitting before the draw has
    /// nothing to pass and fails to compile. See `eqoxide_renderer::DrawnFrame`'s `compile_fail`
    /// doctest, which is the proof that it cannot be fabricated.
    #[test]
    fn a_snapshot_names_the_frame_it_describes_or_admits_there_was_none() {
        let cam = CameraState::new([1.0, 2.0, 3.0], 40.0);
        let resolved = ResolvedEye { eye: [5.0, 6.0, 7.0], occluded: false, still_blocked: false };

        let drawn = cam.snapshot(resolved, eqoxide_renderer::DrawnFrame::for_test(4242));
        assert_eq!(drawn.drawn_frame, Some(4242),
            "a snapshot built from a DrawnFrame token must carry that frame's index — it is the \
             ONLY way a caller can tell a fresh publish from one frozen since the last draw");
        assert!(drawn.drawn_at.is_some(),
            "…and must stamp when that frame was drawn, so `drawn_age_ms` can be computed at read \
             time rather than the caller guessing from an unrelated clock");

        let seed = cam.undrawn_snapshot(resolved);
        assert_eq!(seed.drawn_frame, None,
            "the pre-first-frame seed main.rs publishes must say NO frame has been drawn (#867 \
             blocking 2): it is served through GPU init and the whole first zone load, and a \
             `Some(_)` here would name a frame that never happened");
        assert!(seed.drawn_at.is_none(),
            "…and must have no draw timestamp, so `drawn_age_ms` is null rather than an age since \
             process start masquerading as an age since a draw");

        // The two constructors must agree on everything EXCEPT the freshness pair — otherwise this
        // test could pass with `snapshot` having quietly stopped carrying `resolved` through.
        assert_eq!(drawn.eye, seed.eye);
        assert_eq!(drawn.radius, seed.radius);
    }
}
