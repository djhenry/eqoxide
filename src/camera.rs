/// EQ coordinate system: Z-up. North=+Y. East=+X. Heading 0=north, increases clockwise.
///
/// Third-person follow camera. Returns (camera_pos, look_target).
#[allow(dead_code)]
pub fn follow_camera(player_pos: [f32; 3], heading_deg: f32) -> ([f32; 3], [f32; 3]) {
    let angle = (90.0_f32 - heading_deg).to_radians();
    let back_dist = 80.0_f32;
    let height    = 40.0_f32;
    let cam_x = player_pos[0] - back_dist * angle.cos();
    let cam_y = player_pos[1] - back_dist * angle.sin();
    let cam_z = player_pos[2] + height;
    ([cam_x, cam_y, cam_z], player_pos)
}

/// Perspective view-projection matrix (right-handed, depth 0..1).
pub fn look_at_perspective(
    pos: [f32; 3], target: [f32; 3], up: [f32; 3],
    fov_y_deg: f32, aspect: f32, near: f32, far: f32,
) -> [[f32; 4]; 4] {
    let proj = glam::Mat4::perspective_rh(fov_y_deg.to_radians(), aspect, near, far);
    let view = glam::Mat4::look_at_rh(
        glam::Vec3::from(pos), glam::Vec3::from(target), glam::Vec3::from(up),
    );
    (proj * view).to_cols_array_2d()
}

/// Model matrix: translate to pos lifted by scale*0.5, yaw toward camera, uniform scale.
/// Applies +90° X rotation to convert glTF Y-up models to EQ Z-up world space.
#[allow(dead_code)]
pub fn entity_model_matrix(pos: [f32; 3], cam_pos: [f32; 3], scale: f32) -> [[f32; 4]; 4] {
    entity_model_matrix_scaled(pos, cam_pos, scale, scale, [0.0, 0.0])
}

/// Like entity_model_matrix but with separate visual_scale (for Z-lift) and mesh_scale (for S),
/// plus an optional (x_center, z_center) correction for models not centered at their origin.
///
/// Many glTF models have raw vertices with Z range [0, depth] rather than [-depth/2, depth/2].
/// Passing center_xz = [x_center_raw, z_center_raw] shifts the model by (-x_center, 0, -z_center)
/// in raw model space (before scale), so the rendered model is centered on the entity position.
///
/// Pass center_xz = [0.0, 0.0] for models that are already origin-centered.
#[allow(dead_code)]
pub fn entity_model_matrix_scaled(
    pos: [f32; 3], cam_pos: [f32; 3], visual_scale: f32, mesh_scale: f32,
    center_xz: [f32; 2],
) -> [[f32; 4]; 4] {
    let p     = glam::Vec3::from(pos);
    let delta = glam::Vec3::from(cam_pos) - p;
    let yaw   = delta.y.atan2(delta.x);
    let lifted = p + glam::Vec3::new(0.0, 0.0, visual_scale * 0.5);
    (glam::Mat4::from_translation(lifted)
        * glam::Mat4::from_rotation_z(yaw)
        * glam::Mat4::from_rotation_x(std::f32::consts::FRAC_PI_2)
        * glam::Mat4::from_scale(glam::Vec3::splat(mesh_scale))
        * glam::Mat4::from_translation(glam::Vec3::new(-center_xz[0], 0.0, -center_xz[1])))
    .to_cols_array_2d()
}

/// Model matrix for 3D characters oriented by their EQ heading (not toward the camera).
///
/// `y_up` controls the glTF Y-up → EQ Z-up conversion (a +90° X rotation):
///   - Static models store raw Y-up vertices and need the conversion (`y_up = true`).
///   - Skinned models are already Z-up after skinning: their `CharacterArmature` node
///     carries a baked −90° X rotation that our joint hierarchy excludes, so the
///     skinning math already lands them Z-up. Applying the conversion again would tip
///     them flat onto the ground (`y_up = false`).
///
/// Heading: with the X conversion the model's forward is -Y, so yaw = π − heading places
/// it along the EQ heading. Without the conversion (skinned) the forward is also -Y in the
/// XY plane (the armature leak rotates the model's facing into the same frame), so the same
/// yaw formula applies.
pub fn entity_model_matrix_heading(
    pos: [f32; 3], heading_deg: f32, visual_scale: f32, mesh_scale: f32,
    center_xz: [f32; 2], y_up: bool, y_bottom: f32,
) -> [[f32; 4]; 4] {
    let p      = glam::Vec3::from(pos);
    let yaw    = std::f32::consts::PI - heading_deg.to_radians();
    let lifted = p + glam::Vec3::new(0.0, 0.0, visual_scale * 0.5 + y_bottom * mesh_scale);
    let x_rot  = if y_up { glam::Mat4::from_rotation_x(std::f32::consts::FRAC_PI_2) }
                 else    { glam::Mat4::IDENTITY };
    // `center_xz` holds the two horizontal-axis centers in load order. The model's
    // height axis differs between the two paths, so the recentre translate must
    // leave the height axis untouched (grounding handles vertical placement):
    //   - static (y_up): the raw mesh is Y-up before the +90° X rotation, so the
    //     horizontal axes are X and Z → translate (-c0, 0, -c1).
    //   - skinned (!y_up): the skinned vertices are already Z-up, so the horizontal
    //     axes are X and Y → translate (-c0, -c1, 0).
    let recenter = if y_up {
        glam::Mat4::from_translation(glam::Vec3::new(-center_xz[0], 0.0, -center_xz[1]))
    } else {
        glam::Mat4::from_translation(glam::Vec3::new(-center_xz[0], -center_xz[1], 0.0))
    };
    (glam::Mat4::from_translation(lifted)
        * glam::Mat4::from_rotation_z(yaw)
        * x_rot
        * glam::Mat4::from_scale(glam::Vec3::splat(mesh_scale))
        * recenter)
    .to_cols_array_2d()
}

/// Project world-space position to screen pixels. None if behind camera or outside depth.
pub fn project_to_screen(
    world_pos: [f32; 3],
    view_proj: [[f32; 4]; 4],
    screen_w: u32,
    screen_h: u32,
) -> Option<[f32; 2]> {
    let clip = glam::Mat4::from_cols_array_2d(&view_proj)
        * glam::Vec4::new(world_pos[0], world_pos[1], world_pos[2], 1.0);
    if clip.w <= 0.0 { return None; }
    let ndc_z = clip.z / clip.w;
    if !(0.0..=1.0).contains(&ndc_z) { return None; }
    Some([
        (clip.x / clip.w * 0.5 + 0.5) * screen_w as f32,
        (1.0 - (clip.y / clip.w * 0.5 + 0.5)) * screen_h as f32,
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follow_camera_returns_position_above_and_behind_player() {
        let (cam_pos, target) = follow_camera([0.0, 0.0, 0.0], 0.0);
        assert!(cam_pos[2] > 0.0, "camera should be above player");
        assert_eq!(target, [0.0, 0.0, 0.0]);
    }

    /// Apply a model matrix (column-major 4x4) to a point.
    fn apply(m: &[[f32; 4]; 4], v: [f32; 3]) -> [f32; 3] {
        let x = m[0][0]*v[0] + m[1][0]*v[1] + m[2][0]*v[2] + m[3][0];
        let y = m[0][1]*v[0] + m[1][1]*v[1] + m[2][1]*v[2] + m[3][1];
        let z = m[0][2]*v[0] + m[1][2]*v[1] + m[2][2]*v[2] + m[3][2];
        [x, y, z]
    }

    #[test]
    fn static_model_y_up_axis_maps_to_world_up() {
        // y_up=true: a static model's +Y (its up axis) must convert to world +Z.
        let m = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 0.0, 1.0, [0.0, 0.0], true, 0.0);
        let up = apply(&m, [0.0, 1.0, 0.0]);
        assert!(up[2] > 0.9, "static +Y should map to world +Z (got {up:?})");
    }

    #[test]
    fn skinned_model_z_up_axis_stays_world_up() {
        // y_up=false: a skinned model is already Z-up; its +Z must stay world +Z,
        // and its +Y must stay horizontal (NOT tip up). This guards the
        // double-rotation regression that laid characters flat on the ground.
        let m = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 0.0, 1.0, [0.0, 0.0], false, 0.0);
        let up = apply(&m, [0.0, 0.0, 1.0]);
        assert!(up[2] > 0.9, "skinned +Z should stay world +Z (got {up:?})");
        let fwd = apply(&m, [0.0, 1.0, 0.0]);
        assert!(fwd[2].abs() < 0.1, "skinned +Y must stay horizontal, not tip up (got {fwd:?})");
    }

    #[test]
    fn recenter_never_changes_world_height() {
        // The horizontal recentre must never shift a vertex along world Z (height),
        // for either path. The bug that buried the Skeleton applied the skinned model's
        // depth-axis centre as a vertical shift. Verify world Z is invariant to the
        // recentre values for an arbitrary vertex.
        let v = [0.004_f32, -0.003, 0.012];
        for y_up in [true, false] {
            let m0 = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 1.0, 2600.0, [0.0, 0.0], y_up, 0.0);
            let m1 = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 1.0, 2600.0, [3.0, 5.0], y_up, 0.0);
            let z0 = apply(&m0, v)[2];
            let z1 = apply(&m1, v)[2];
            assert!((z0 - z1).abs() < 1e-3,
                "y_up={y_up}: recentre changed world height ({z0} vs {z1})");
        }
    }

    #[test]
    fn follow_camera_heading_east_places_camera_west_of_player() {
        let player = [100.0_f32, 200.0_f32, 10.0_f32];
        let (cam, target) = follow_camera(player, 90.0);
        assert!(cam[0] < player[0], "camera should be west of player when facing east");
        assert_eq!(target, player);
    }

    #[test]
    fn look_at_perspective_returns_nonzero_4x4() {
        let m = look_at_perspective(
            [0.0, -80.0, 40.0], [0.0, 0.0, 0.0], [0.0, 0.0, 1.0],
            60.0, 1.333, 0.5, 5000.0,
        );
        assert_eq!(m.len(), 4);
        let sum: f32 = m.iter().flatten().map(|x| x.abs()).sum();
        assert!(sum > 0.0);
    }

    #[test]
    fn project_to_screen_front_maps_to_center() {
        let vp = look_at_perspective(
            [0.0, 0.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0],
            60.0, 1.0, 0.1, 100.0,
        );
        let result = project_to_screen([0.0, 0.0, 0.0], vp, 800, 600);
        assert!(result.is_some());
        let [sx, sy] = result.unwrap();
        assert!((sx - 400.0).abs() < 5.0, "got {sx}");
        assert!((sy - 300.0).abs() < 5.0, "got {sy}");
    }

    #[test]
    fn project_to_screen_behind_camera_returns_none() {
        let vp = look_at_perspective(
            [0.0, 0.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0],
            60.0, 1.0, 0.1, 100.0,
        );
        assert!(project_to_screen([0.0, 0.0, 20.0], vp, 800, 600).is_none());
    }

    #[test]
    fn entity_model_matrix_is_4x4_nonzero() {
        let m = entity_model_matrix([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], 1.0);
        assert_eq!(m.len(), 4);
        let sum: f32 = m.iter().flatten().map(|x| x.abs()).sum();
        assert!(sum > 0.0);
    }

    #[test]
    fn entity_model_matrix_scaled_lift_uses_visual_not_mesh_scale() {
        // camera at [0,-80,40], entity at [0,0,0], visual_scale=54, mesh_scale=5400
        // The center of the model should be lifted by visual_scale*0.5=27, NOT mesh_scale*0.5=2700.
        let m = entity_model_matrix_scaled(
            [0.0, 0.0, 0.0], [0.0, -80.0, 40.0], 54.0, 5400.0, [0.0, 0.0]
        );
        // Column 3 of the matrix = translation column. m[3][2] = z-translation.
        let z_translation = m[3][2];
        assert!(
            (z_translation - 27.0).abs() < 1.0,
            "z-lift should be visual_scale*0.5=27, got {z_translation} (mesh_scale*0.5 would be 2700)"
        );
    }

    #[test]
    fn entity_model_matrix_scaled_center_is_within_standard_frustum() {
        // Regression: skinned models with node_scale=100 were lifted to z=2700, outside frustum.
        // The model center (lifted position) must be visible from the follow camera.
        let cam_pos = [0.0_f32, -80.0, 40.0];
        let entity_pos = [0.0_f32, 15.0, 0.0];
        let visual_scale = 54.0_f32;
        let mesh_scale = 54.0 * 100.0_f32;
        let mat = entity_model_matrix_scaled(entity_pos, cam_pos, visual_scale, mesh_scale, [0.0, 0.0]);
        // Model center = entity_pos + z_lift. m[3] is the translation column.
        let center_z = mat[3][2];
        // Camera z is 40. Far plane is 5000. Center must be reachable.
        assert!(center_z < 5000.0,
            "model center z={center_z} exceeds far plane");
        // And the dot product of (center - camera) with view direction must be positive.
        let view_dir = {
            let dx = 0.0_f32 - cam_pos[0];
            let dy = 0.0_f32 - cam_pos[1];
            let dz = 0.0_f32 - cam_pos[2];
            let len = (dx*dx + dy*dy + dz*dz).sqrt();
            [dx/len, dy/len, dz/len]
        };
        let center_world = [mat[3][0], mat[3][1], center_z];
        let to_center = [
            center_world[0] - cam_pos[0],
            center_world[1] - cam_pos[1],
            center_world[2] - cam_pos[2],
        ];
        let dot = to_center[0]*view_dir[0] + to_center[1]*view_dir[1] + to_center[2]*view_dir[2];
        assert!(dot > 0.0, "model center is behind the camera (dot={dot})");
    }
}
