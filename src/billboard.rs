use crate::gpu::Vertex;

/// Camera-facing quad at `pos`. Returns 4 vertices + 6 indices.
/// The `normal` field of each vertex carries `color` (billboard shader reads it as color).
pub fn billboard_quad(
    pos: [f32; 3],
    size: f32,
    color: [f32; 3],
    cam_right: [f32; 3],
    cam_up:    [f32; 3],
) -> (Vec<Vertex>, Vec<u32>) {
    let right = glam::Vec3::from(cam_right) * size;
    let up    = glam::Vec3::from(cam_up)    * size;
    let c     = glam::Vec3::from(pos);
    let corners = [c - right - up, c + right - up, c + right + up, c - right + up];
    let vertices = corners.iter().map(|p| Vertex {
        position: p.to_array(),
        normal:   color,
        uv:       [0.0, 0.0],
    }).collect();
    (vertices, vec![0, 1, 2, 0, 2, 3])
}

/// Billboard color from entity state. Priority: dead > target > low HP > default.
pub fn npc_color(is_target: bool, dead: bool, hp_pct: f32) -> [f32; 3] {
    if dead            { return [0.5, 0.5, 0.5]; }   // grey
    if is_target       { return [0.97, 0.32, 0.29]; } // red
    if hp_pct < 30.0   { return [0.97, 0.32, 0.29]; } // red (low HP)
    [1.0, 0.65, 0.34]                                  // orange
}

/// Billboard half-width: scales from 3 units (level 1) to 10 units (level 40+).
pub fn npc_size(level: u32) -> f32 {
    3.0 + (level.min(40) as f32 / 40.0) * 7.0
}

/// Ground-level X marker for level-0 placeholder spawns.
/// Two thin quads crossing at `pos`, lying flat on the XY plane.
pub fn cross_marker(pos: [f32; 3], size: f32, color: [f32; 3]) -> (Vec<Vertex>, Vec<u32>) {
    let half = size * 0.5;
    let arm  = size * 0.08; // thin arm width
    let z    = pos[2] + 0.1; // slightly above ground to avoid z-fighting
    let [px, py, _] = pos;

    // Arm 1: diagonal from (-half, -half) to (+half, +half)
    // Perpendicular offset for arm width: (-1,1) normalized
    let (dx1, dy1) = (1.0_f32, 1.0_f32);
    let len1 = (dx1 * dx1 + dy1 * dy1).sqrt();
    let (nx1, ny1) = (-dy1 / len1 * arm, dx1 / len1 * arm);

    // Arm 2: diagonal from (-half, +half) to (+half, -half)
    let (dx2, dy2) = (1.0_f32, -1.0_f32);
    let len2 = (dx2 * dx2 + dy2 * dy2).sqrt();
    let (nx2, ny2) = (-dy2 / len2 * arm, dx2 / len2 * arm);

    let v = |x: f32, y: f32| Vertex {
        position: [x, y, z], normal: color, uv: [0.0; 2],
    };

    let verts = vec![
        // Arm 1
        v(px - half * dx1 + nx1, py - half * dy1 + ny1),
        v(px - half * dx1 - nx1, py - half * dy1 - ny1),
        v(px + half * dx1 + nx1, py + half * dy1 + ny1),
        v(px + half * dx1 - nx1, py + half * dy1 - ny1),
        // Arm 2
        v(px - half * dx2 + nx2, py - half * dy2 + ny2),
        v(px - half * dx2 - nx2, py - half * dy2 - ny2),
        v(px + half * dx2 + nx2, py + half * dy2 + ny2),
        v(px + half * dx2 - nx2, py + half * dy2 - ny2),
    ];
    let idxs: Vec<u32> = vec![
        0, 1, 2, 1, 2, 3,  // arm 1
        4, 5, 6, 5, 6, 7,  // arm 2
    ];
    (verts, idxs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn billboard_quad_returns_4_vertices_6_indices() {
        let (verts, idxs) = billboard_quad(
            [0.0, 0.0, 0.0], 5.0, [1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0], [0.0, 0.0, 1.0],
        );
        assert_eq!(verts.len(), 4);
        assert_eq!(idxs.len(), 6);
    }

    #[test]
    fn npc_color_dead_is_grey() {
        let c = npc_color(false, true, 100.0);
        assert!((c[0] - 0.5).abs() < 0.01 && (c[1] - 0.5).abs() < 0.01);
    }

    #[test]
    fn npc_color_target_is_red() {
        let c = npc_color(true, false, 80.0);
        assert!(c[0] > 0.9, "expected red, got {:?}", c);
    }

    #[test]
    fn npc_size_scales_with_level() {
        assert!(npc_size(40) > npc_size(1));
    }

    #[test]
    fn npc_size_level_above_40_capped() {
        assert!((npc_size(60) - 10.0).abs() < 0.01);
        assert!((npc_size(40) - 10.0).abs() < 0.01);
    }

    #[test]
    fn cross_marker_returns_8_vertices_12_indices() {
        let (verts, idxs) = cross_marker([1.0, 2.0, 3.0], 4.0, [0.9, 0.2, 0.2]);
        assert_eq!(verts.len(), 8);
        assert_eq!(idxs.len(), 12);
    }

    #[test]
    fn cross_marker_z_above_input() {
        let (verts, _) = cross_marker([0.0, 0.0, 5.0], 4.0, [1.0, 0.0, 0.0]);
        for v in &verts {
            assert!(v.position[2] > 5.0, "cross marker Z should be above ground");
        }
    }
}
