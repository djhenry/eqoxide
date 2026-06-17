pub const ELEVATION_MIN: f32 = 0.08727; // 5°
pub const ELEVATION_MAX: f32 = 1.39626; // 80°
pub const RADIUS_MIN:    f32 = 20.0;
pub const RADIUS_MAX:    f32 = 500.0;
const DESIRED_ELEVATION: f32 = 0.69813; // 40°
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
/// (EQ convention: 0=north/+Y, clockwise).
pub fn desired_azimuth(heading_deg: f32) -> f32 {
    let h = heading_deg.to_radians();
    f32::atan2(-h.cos(), -h.sin())
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraMode { AutoFollow, ManualOrbit }

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

#[derive(Debug, Clone, serde::Serialize)]
pub struct CameraSnapshot {
    pub mode:      CameraMode,
    pub azimuth:   f32,
    pub elevation: f32,
    pub radius:    f32,
    pub focus:     [f32; 3],
}

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

        match self.mode {
            CameraMode::ManualOrbit => {}
            CameraMode::AutoFollow => {
                let alpha     = 1.0 - (-FOLLOW_RATE * dt).exp();
                self.focus    = lerp3(self.focus, player_pos, alpha);
                self.azimuth  = des_az;
                self.elevation = DESIRED_ELEVATION;
                self.radius   = DESIRED_RADIUS;
            }
        }

        let eye = compute_eye(self.azimuth, self.elevation, self.radius, self.focus);
        let look = [self.focus[0], self.focus[1], self.focus[2] + LOOK_TARGET_Z];
        (eye, look)
    }

    /// Adjust azimuth and elevation from mouse drag (radians/pixel × pixel-delta).
    pub fn apply_orbit_delta(&mut self, daz: f32, del: f32) {
        self.azimuth   = (self.azimuth + daz).rem_euclid(std::f32::consts::TAU);
        self.elevation = (self.elevation - del).clamp(ELEVATION_MIN, ELEVATION_MAX);
        self.mode      = CameraMode::ManualOrbit;
    }

    /// Zoom by `factor` scroll lines (positive = zoom in).
    pub fn apply_zoom(&mut self, factor: f32) {
        self.radius = (self.radius * (1.0 - factor)).clamp(RADIUS_MIN, RADIUS_MAX);
        self.mode   = CameraMode::ManualOrbit;
    }

    /// Instantly snap back to AutoFollow (R/F9 key or HTTP reset).
    pub fn reset_to_follow(&mut self) {
        self.mode = CameraMode::AutoFollow;
    }

    /// Apply an HTTP command to the camera state.
    pub fn apply_cmd(&mut self, cmd: CameraCmd) {
        match cmd {
            CameraCmd::Set { azimuth, elevation, radius, focus } => {
                if let Some(az) = azimuth   { self.azimuth   = az; }
                if let Some(el) = elevation { self.elevation = el.clamp(ELEVATION_MIN, ELEVATION_MAX); }
                if let Some(r)  = radius    { self.radius    = r.clamp(RADIUS_MIN, RADIUS_MAX); }
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
    fn desired_azimuth_heading_east_gives_west_camera() {
        let az = desired_azimuth(90.0);
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
    fn apply_cmd_set_clamps_out_of_range_elevation() {
        let mut cam = CameraState::new([0.0, 0.0, 0.0], 0.0);
        cam.apply_cmd(CameraCmd::Set {
            azimuth: None, elevation: Some(10.0), radius: None, focus: None,
        });
        assert!(cam.elevation <= ELEVATION_MAX + 1e-5,
            "elevation {:.3} exceeds max {:.3}", cam.elevation, ELEVATION_MAX);
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
