//! Camera math: the EQ coordinate conventions, the third-person follow camera, the view/projection
//! matrices (note the clip-space X flip that un-mirrors the world), and world→screen projection used
//! to place nameplates/labels in the HUD.

/// EQ coordinate system: Z-up. North=+Y. East=+X. Heading 0=north, increases CCW.
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
    // EQ coords are +X=west/+Y=north; our right-handed camera would draw +X (west) to screen
    // right, producing a left-right mirrored world (clock-tower door on the wrong side). The
    // whole scene (geometry + entities) is internally aligned, so we correct the *display* by
    // negating clip-space X. Safe because all pipelines use cull_mode:None (no winding flip).
    let flip = glam::Mat4::from_scale(glam::Vec3::new(-1.0, 1.0, 1.0));
    (flip * proj * view).to_cols_array_2d()
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
/// Heading is CCW (0=north, 90=west). The glTF models' front faces +X in local
/// space, so yaw = heading_rad + π/2 rotates +X to the CCW heading direction.
pub fn entity_model_matrix_heading(
    pos: [f32; 3], heading_deg: f32, visual_scale: f32, mesh_scale: f32,
    center_xz: [f32; 2], y_up: bool, y_bottom: f32, correction: glam::Mat4,
) -> [[f32; 4]; 4] {
    let p      = glam::Vec3::from(pos);
    let yaw    = heading_deg.to_radians() + std::f32::consts::FRAC_PI_2;
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
    // `correction` is a per-archetype model-space fix-up (identity for most): some substitute GLB
    // models are authored with a body axis that leaves them mis-oriented after the standard
    // conversion (e.g. the shared `fish.glb` renders nose-down). Applied AFTER the Y-up/Z-up
    // conversion and BEFORE the heading yaw, so it re-orients the model into the canonical
    // "front = +X, flat on the ground" pose the yaw then points by heading (#149).
    (glam::Mat4::from_translation(lifted)
        * glam::Mat4::from_rotation_z(yaw)
        * correction
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

/// Unit movement direction for mouse-look "drive" mode, in world [east, north, z].
///
/// Horizontal component is the camera's forward look direction (toward the focus):
/// `(-cos az, -sin az)`. When `vertical` is true (player swimming in water), the result
/// is the full 3D look direction so the player swims up/down by the camera pitch; the
/// vertical term is `-sin(el)` (the eye sits `+sin(el)` above the focus, so the look
/// direction points down by that much). When `vertical` is false the z term is 0 and the
/// player moves horizontally only. The returned vector is unit length in both cases.
pub fn camera_move_dir(azimuth: f32, elevation: f32, vertical: bool) -> [f32; 3] {
    if vertical {
        let (sin_el, cos_el) = elevation.sin_cos();
        [-cos_el * azimuth.cos(), -cos_el * azimuth.sin(), -sin_el]
    } else {
        [-azimuth.cos(), -azimuth.sin(), 0.0]
    }
}

/// Ray vs axis-aligned box (slab method). Returns the nearest non-negative hit distance `t`
/// (parameter along `dir`, which need NOT be unit length), or `None` if the ray misses or the
/// box is entirely behind the origin. If the origin is inside the box, returns 0. Used for
/// door click-picking: transform the world ray into a door's local space, then test its AABB.
pub fn ray_aabb(origin: [f32; 3], dir: [f32; 3], min: [f32; 3], max: [f32; 3]) -> Option<f32> {
    let mut tmin = f32::NEG_INFINITY;
    let mut tmax = f32::INFINITY;
    for k in 0..3 {
        if dir[k].abs() < 1e-9 {
            // Ray parallel to this slab: miss unless the origin is within the slab.
            if origin[k] < min[k] || origin[k] > max[k] { return None; }
        } else {
            let inv = 1.0 / dir[k];
            let mut t1 = (min[k] - origin[k]) * inv;
            let mut t2 = (max[k] - origin[k]) * inv;
            if t1 > t2 { std::mem::swap(&mut t1, &mut t2); }
            tmin = tmin.max(t1);
            tmax = tmax.min(t2);
            if tmin > tmax { return None; }
        }
    }
    if tmax < 0.0 { return None; }
    Some(tmin.max(0.0))
}

/// Visibility test for entity culling. Returns true if an entity standing at `pos`
/// (its feet) should be rendered, given the player position and the view-projection
/// matrix. Culls in two cheap ways:
///   * Distance: farther than `max_dist` from `player_pos` (3D) → not drawn.
///   * Frustum:  behind the camera, beyond the far plane, or outside the NDC box
///     (with `margin` slack on x/y so a tall model whose feet sit just off-screen
///     is still drawn; the position is the feet, the body extends upward/sideways).
///
/// `margin` is in NDC units (1.0 = a full half-screen of slack).
pub fn entity_in_view(
    pos:        [f32; 3],
    player_pos: [f32; 3],
    view_proj:  [[f32; 4]; 4],
    max_dist:   f32,
    margin:     f32,
) -> bool {
    let dx = pos[0] - player_pos[0];
    let dy = pos[1] - player_pos[1];
    let dz = pos[2] - player_pos[2];
    if dx * dx + dy * dy + dz * dz > max_dist * max_dist {
        return false;
    }

    let clip = glam::Mat4::from_cols_array_2d(&view_proj)
        * glam::Vec4::new(pos[0], pos[1], pos[2], 1.0);
    if clip.w <= 0.0 {
        return false; // behind the camera
    }
    let inv_w = 1.0 / clip.w;
    let ndc_x = clip.x * inv_w;
    let ndc_y = clip.y * inv_w;
    let ndc_z = clip.z * inv_w;
    // Far-plane cull (ndc_z > 1). Don't cull on the near side: a tall model whose
    // feet are behind the near plane can still be visible, and the w<=0 test above
    // already removes anything truly behind the camera.
    if ndc_z > 1.0 {
        return false;
    }
    ndc_x.abs() <= 1.0 + margin && ndc_y.abs() <= 1.0 + margin
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
        let m = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 0.0, 1.0, [0.0, 0.0], true, 0.0, glam::Mat4::IDENTITY);
        let up = apply(&m, [0.0, 1.0, 0.0]);
        assert!(up[2] > 0.9, "static +Y should map to world +Z (got {up:?})");
    }

    #[test]
    fn skinned_model_z_up_axis_stays_world_up() {
        // y_up=false: a skinned model is already Z-up; its +Z must stay world +Z,
        // and its +Y must stay horizontal (NOT tip up). This guards the
        // double-rotation regression that laid characters flat on the ground.
        let m = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 0.0, 1.0, [0.0, 0.0], false, 0.0, glam::Mat4::IDENTITY);
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
            let m0 = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 1.0, 2600.0, [0.0, 0.0], y_up, 0.0, glam::Mat4::IDENTITY);
            let m1 = entity_model_matrix_heading([0.0, 0.0, 0.0], 0.0, 1.0, 2600.0, [3.0, 5.0], y_up, 0.0, glam::Mat4::IDENTITY);
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

    fn cull_vp() -> [[f32; 4]; 4] {
        // Camera at +Z looking at origin (mirrors the project_to_screen tests).
        look_at_perspective([0.0, 0.0, 5.0], [0.0, 0.0, 0.0], [0.0, 1.0, 0.0],
                            60.0, 1.0, 0.1, 1000.0)
    }

    #[test]
    fn entity_in_view_centered_is_visible() {
        assert!(entity_in_view([0.0, 0.0, 0.0], [0.0, 0.0, 0.0], cull_vp(), 500.0, 0.5));
    }

    #[test]
    fn ray_aabb_hits_box_ahead() {
        // Ray from -10 on X heading +X into a unit box at origin: enters at x=-1 => t=9.
        let t = ray_aabb([-10.0, 0.0, 0.0], [1.0, 0.0, 0.0], [-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        assert!(t.is_some());
        assert!((t.unwrap() - 9.0).abs() < 1e-4, "t={:?}", t);
    }

    #[test]
    fn ray_aabb_misses_box_to_the_side() {
        // Parallel offset above the box on Z: never enters.
        let t = ray_aabb([-10.0, 0.0, 5.0], [1.0, 0.0, 0.0], [-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        assert!(t.is_none(), "expected miss, got {:?}", t);
    }

    #[test]
    fn ray_aabb_box_behind_origin_is_none() {
        // Box is behind the ray (heading +X away from a box at negative X).
        let t = ray_aabb([10.0, 0.0, 0.0], [1.0, 0.0, 0.0], [-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        assert!(t.is_none(), "expected none, got {:?}", t);
    }

    #[test]
    fn ray_aabb_origin_inside_returns_zero() {
        let t = ray_aabb([0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]);
        assert_eq!(t, Some(0.0));
    }

    #[test]
    fn ray_aabb_larger_box_is_easier_to_hit_than_small() {
        // A glancing ray that misses a small box hits a tall one (the door-size fix in miniature).
        let origin = [-10.0, 0.0, 3.0];
        let dir = [1.0, 0.0, 0.0];
        assert!(ray_aabb(origin, dir, [-1.0, -1.0, -1.0], [1.0, 1.0, 1.0]).is_none());
        assert!(ray_aabb(origin, dir, [-1.0, -1.0, -8.0], [1.0, 1.0, 8.0]).is_some());
    }

    #[test]
    fn entity_in_view_beyond_draw_distance_is_culled() {
        // In the frustum direction but past max_dist → culled by distance.
        let far = [0.0, 0.0, -600.0];
        assert!(!entity_in_view(far, [0.0, 0.0, 0.0], cull_vp(), 500.0, 0.5));
    }

    #[test]
    fn entity_in_view_behind_camera_is_culled() {
        // Near enough in distance, but behind the camera (camera looks toward -Z).
        let behind = [0.0, 0.0, 20.0];
        assert!(!entity_in_view(behind, [0.0, 0.0, 18.0], cull_vp(), 500.0, 0.5));
    }

    #[test]
    fn camera_move_dir_horizontal_is_unit_and_flat() {
        let d = camera_move_dir(0.7, 0.5, false);
        assert!((d[2]).abs() < 1e-6, "horizontal move must have no z");
        let len = (d[0]*d[0] + d[1]*d[1] + d[2]*d[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "len={len}");
    }

    #[test]
    fn camera_move_dir_vertical_is_unit_3d() {
        let d = camera_move_dir(0.7, 0.5, true);
        let len = (d[0]*d[0] + d[1]*d[1] + d[2]*d[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-5, "len={len}");
    }

    #[test]
    fn camera_move_dir_positive_elevation_points_down() {
        // Eye above focus (el>0) => looking down => swim direction has negative z.
        let d = camera_move_dir(0.0, 0.6, true);
        assert!(d[2] < 0.0, "expected downward z, got {}", d[2]);
    }

    #[test]
    fn camera_move_dir_negative_elevation_points_up() {
        // Eye below focus (el<0) => looking up => swim up.
        let d = camera_move_dir(0.0, -0.6, true);
        assert!(d[2] > 0.0, "expected upward z, got {}", d[2]);
    }

    #[test]
    fn entity_in_view_far_to_the_side_is_culled() {
        // Close in distance but way off to the side (outside frustum x).
        let side = [50.0, 0.0, 0.0];
        assert!(!entity_in_view(side, [0.0, 0.0, 0.0], cull_vp(), 500.0, 0.5));
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
            let len = (dx*dx + dy*dy + dz*dz).sqrt().max(1e-6);
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
