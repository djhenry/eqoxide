//! Orbit/follow camera state: azimuth/elevation/radius with smoothing toward the player, mouse-drag
//! and scroll input, and the `CameraCmd`/`CameraSnapshot` types shared with the HTTP `/camera`
//! endpoint (the agent sets a command; the render loop applies it and publishes a snapshot back).
//!
//! `CameraMode`/`CameraCmd`/`CameraSnapshot` are pure inter-thread contract data — they moved DOWN
//! into `eqoxide-ipc` (#544 Step 2c) so that crate's `CameraSlots` no longer up-references
//! `camera_state`. The BEHAVIOR (`CameraState` and its update/snapshot logic) stays here and `use`s
//! those types. Re-exported so every existing `crate::camera_state::{CameraMode,CameraCmd,
//! CameraSnapshot}` path across the tree keeps resolving unchanged.
pub use eqoxide_ipc::{CameraCmd, CameraMode, CameraSnapshot};

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

/// Camera azimuth that places the camera behind a player facing `heading_deg`
/// (EQ convention: 0=north/+Y, CCW). Camera sits opposite the facing direction:
///   az = heading_rad - π/2
pub fn desired_azimuth(heading_deg: f32) -> f32 {
    heading_deg.to_radians() - std::f32::consts::FRAC_PI_2
}

/// Inverse of [`desired_azimuth`]: the heading (EQ degrees, CCW) a character must face
/// to be looking in the camera's horizontal direction. Used by mouse-look "drive" mode,
/// where the character's heading is slaved to the camera while LMB + a move key are held.
pub fn heading_deg_from_azimuth(azimuth: f32) -> f32 {
    (azimuth + std::f32::consts::FRAC_PI_2).to_degrees().rem_euclid(360.0)
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

        let eye = compute_eye(self.azimuth, self.elevation, self.radius, self.focus);
        let look = [self.focus[0], self.focus[1], self.focus[2] + LOOK_TARGET_Z];
        (eye, look)
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

    /// Snapshot of the current state for the HTTP GET /camera response.
    pub fn snapshot(&self) -> CameraSnapshot {
        CameraSnapshot {
            mode:      self.mode,
            azimuth:   self.azimuth,
            elevation: self.elevation,
            radius:    self.radius,
            focus:     self.focus,
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
}
