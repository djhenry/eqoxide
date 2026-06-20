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

/// TEMP DEBUG: colored XYZ axis gizmo at `pos` (the point a model should be centered on).
/// Red = +X (east), Green = +Y (north), Blue = +Z (up). Color is carried in `normal`
/// (the billboard pipeline reads color from there). Lets us see a model's offset from
/// where it ought to render. Remove when the position bug is resolved.
pub fn axis_gizmo(pos: [f32; 3], len: f32, heading_deg: f32) -> (Vec<Vertex>, Vec<u32>) {
    let w = 0.4;
    let mut verts: Vec<Vertex> = Vec::new();
    let mut idxs: Vec<u32> = Vec::new();
    fn push_box(verts: &mut Vec<Vertex>, idxs: &mut Vec<u32>,
                mn: [f32; 3], mx: [f32; 3], color: [f32; 3]) {
        let base = verts.len() as u32;
        let c = [
            [mn[0],mn[1],mn[2]],[mx[0],mn[1],mn[2]],[mx[0],mx[1],mn[2]],[mn[0],mx[1],mn[2]],
            [mn[0],mn[1],mx[2]],[mx[0],mn[1],mx[2]],[mx[0],mx[1],mx[2]],[mn[0],mx[1],mx[2]],
        ];
        for p in c { verts.push(Vertex { position: p, normal: color, uv: [0.0; 2] }); }
        for f in [[0u32,2,1,0,3,2],[4,5,6,4,6,7],[0,1,5,0,5,4],[1,2,6,1,6,5],[2,3,7,2,7,6],[3,0,4,3,4,7]] {
            for i in f { idxs.push(base + i); }
        }
    }
    let [x, y, z] = pos;
    push_box(&mut verts, &mut idxs, [x, y - w, z - w], [x + len, y + w, z + w], [1.0, 0.0, 0.0]); // +X red
    push_box(&mut verts, &mut idxs, [x - w, y, z - w], [x + w, y + len, z + w], [0.0, 1.0, 0.0]); // +Y green
    push_box(&mut verts, &mut idxs, [x - w, y - w, z], [x + w, y + w, z + len], [0.0, 0.0, 1.0]); // +Z up blue
    // small white cube at the exact origin point
    push_box(&mut verts, &mut idxs, [x - w, y - w, z - w], [x + w, y + w, z + w], [1.0, 1.0, 1.0]);

    // TEMP DEBUG: yellow heading arrow on the ground, pointing the character's facing.
    // EQ heading 0 = north/+Y, CCW (see desired_azimuth): forward = (-sin h, cos h).
    let h = heading_deg.to_radians();
    let (sh, ch) = h.sin_cos();
    let (dx, dy) = (-sh, ch);          // world XY facing direction
    let (px, py) = (-dy, dx);          // perpendicular (for arrow width)
    let alen = len * 1.2;
    let aw = 1.2;
    let zf = z + 0.3;
    let yc = [1.0, 1.0, 0.0];
    let base = verts.len() as u32;
    // shaft quad + triangular head as one flat arrow
    let shaft = alen * 0.7;
    verts.push(Vertex { position: [x + px * aw, y + py * aw, zf], normal: yc, uv: [0.0; 2] });          // 0 base-left
    verts.push(Vertex { position: [x - px * aw, y - py * aw, zf], normal: yc, uv: [0.0; 2] });          // 1 base-right
    verts.push(Vertex { position: [x + dx * shaft + px * aw, y + dy * shaft + py * aw, zf], normal: yc, uv: [0.0; 2] }); // 2
    verts.push(Vertex { position: [x + dx * shaft - px * aw, y + dy * shaft - py * aw, zf], normal: yc, uv: [0.0; 2] }); // 3
    verts.push(Vertex { position: [x + dx * shaft + px * aw * 2.2, y + dy * shaft + py * aw * 2.2, zf], normal: yc, uv: [0.0; 2] }); // 4 head-left
    verts.push(Vertex { position: [x + dx * shaft - px * aw * 2.2, y + dy * shaft - py * aw * 2.2, zf], normal: yc, uv: [0.0; 2] }); // 5 head-right
    verts.push(Vertex { position: [x + dx * alen, y + dy * alen, zf], normal: yc, uv: [0.0; 2] });      // 6 tip
    for i in [0u32,1,2, 1,3,2, 4,5,6] { idxs.push(base + i); }

    // TEMP DEBUG: cyan wireframe box where the model SHOULD be — centered on pos, feet on the
    // ground (z), rising to the target height (len). If the model is placed right it sits
    // inside; offset/float makes it poke out. Drawn as 12 thin edge-boxes.
    let hw = 4.0;            // half-width of the expected footprint
    let ew = 0.15;          // edge thickness
    let cy = [0.0, 1.0, 1.0];
    let (x0, x1, y0, y1, z0, z1) = (x - hw, x + hw, y - hw, y + hw, z, z + len);
    let mut edge = |a: [f32; 3], b: [f32; 3]| {
        let mn = [a[0].min(b[0]) - ew, a[1].min(b[1]) - ew, a[2].min(b[2]) - ew];
        let mx = [a[0].max(b[0]) + ew, a[1].max(b[1]) + ew, a[2].max(b[2]) + ew];
        push_box(&mut verts, &mut idxs, mn, mx, cy);
    };
    // 4 verticals
    edge([x0,y0,z0],[x0,y0,z1]); edge([x1,y0,z0],[x1,y0,z1]);
    edge([x1,y1,z0],[x1,y1,z1]); edge([x0,y1,z0],[x0,y1,z1]);
    // bottom rectangle
    edge([x0,y0,z0],[x1,y0,z0]); edge([x1,y0,z0],[x1,y1,z0]);
    edge([x1,y1,z0],[x0,y1,z0]); edge([x0,y1,z0],[x0,y0,z0]);
    // top rectangle
    edge([x0,y0,z1],[x1,y0,z1]); edge([x1,y0,z1],[x1,y1,z1]);
    edge([x1,y1,z1],[x0,y1,z1]); edge([x0,y1,z1],[x0,y0,z1]);
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
