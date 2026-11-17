//! Door model placement shared by the renderer and by collision.
//!
//! A door glb stores each vertex as `[wld x, wld z (up), wld y]`. The door model space is z-up,
//! so the renderer and collision map a glb vertex through [`door_local_from_glb`] and place the
//! result with [`door_placement`]. Using one function for each keeps the drawn mesh and the
//! collision mesh in the same place.

use crate::MeshData;

/// Returns glb vertex `p` in door model space, which is `[p[2], p[0], p[1]]`.
pub fn door_local_from_glb(p: [f32; 3]) -> [f32; 3] {
    [p[2], p[0], p[1]]
}

/// Returns the matrix that places a door model in the world.
///
/// The matrix translates to `pos`, rotates about z by `heading` (raw EQ units, 0 to 512) plus a
/// quarter turn, rotates about y by `incline` (raw EQ units, 0 to 512) and scales by `size / 100`.
pub fn door_placement(pos: [f32; 3], heading: f32, incline: i32, size: u16) -> glam::Mat4 {
    let yaw = (heading / 512.0) * std::f32::consts::TAU + std::f32::consts::FRAC_PI_2;
    glam::Mat4::from_translation(glam::Vec3::from(pos))
        * glam::Mat4::from_rotation_z(yaw)
        * glam::Mat4::from_rotation_y((incline as f32 / 512.0) * std::f32::consts::TAU)
        * glam::Mat4::from_scale(glam::Vec3::splat(size as f32 / 100.0))
}

/// Returns every triangle of `meshes` in world space, after `placement` and a lift of `rise`
/// units along world z.
pub fn door_world_tris(meshes: &[MeshData], placement: &glam::Mat4, rise: f32) -> Vec<[[f32; 3]; 3]> {
    let world = |mesh: &MeshData, i: u32| {
        let p = mesh.positions[i as usize];
        let local = door_local_from_glb([
            p[0] + mesh.center[0], p[1] + mesh.center[1], p[2] + mesh.center[2],
        ]);
        let mut w = placement.transform_point3(glam::Vec3::from(local)).to_array();
        w[2] += rise;
        w
    };
    meshes.iter()
        .flat_map(|m| m.indices.chunks_exact(3).map(move |t| [world(m, t[0]), world(m, t[1]), world(m, t[2])]))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RenderMode;

    fn mesh(positions: Vec<[f32; 3]>, indices: Vec<u32>, center: [f32; 3]) -> MeshData {
        MeshData {
            normals: vec![[0.0, 0.0, 1.0]; positions.len()], uvs: vec![[0.0, 0.0]; positions.len()],
            positions, indices, texture_name: None, base_color: [1.0; 4], center,
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    #[test]
    fn door_local_maps_glb_axes() {
        assert_eq!(door_local_from_glb([1.0, 2.0, 3.0]), [3.0, 1.0, 2.0]);
    }

    #[test]
    fn heading_zero_turns_local_x_to_world_y() {
        let m = door_placement([10.0, 20.0, 30.0], 0.0, 0, 100);
        let p = m.transform_point3(glam::Vec3::X).to_array();
        assert!((p[0] - 10.0).abs() < 1e-4 && (p[1] - 21.0).abs() < 1e-4 && (p[2] - 30.0).abs() < 1e-4, "{p:?}");
    }

    #[test]
    fn size_scales_the_model() {
        let m = door_placement([0.0; 3], 0.0, 0, 200);
        let p = m.transform_point3(glam::Vec3::Z).to_array();
        assert!((p[2] - 2.0).abs() < 1e-4, "{p:?}");
    }

    #[test]
    fn world_tris_apply_the_axis_map_the_placement_and_the_rise() {
        // glb vertex [0, 1, 0] is one unit up, so it is local z 1
        let m = mesh(vec![[0.0, 1.0, 0.0], [0.0, 1.0, 0.0], [0.0, 1.0, 0.0]], vec![0, 1, 2], [0.0; 3]);
        let tris = door_world_tris(&[m], &door_placement([5.0, 6.0, 7.0], 0.0, 0, 100), 4.0);
        assert_eq!(tris.len(), 1);
        let v = tris[0][0];
        assert!((v[0] - 5.0).abs() < 1e-4 && (v[1] - 6.0).abs() < 1e-4 && (v[2] - 12.0).abs() < 1e-4, "{v:?}");
    }

    #[test]
    fn world_tris_include_the_mesh_center() {
        let m = mesh(vec![[0.0; 3]; 3], vec![0, 1, 2], [0.0, 2.0, 0.0]);
        let v = door_world_tris(&[m], &glam::Mat4::IDENTITY, 0.0)[0][0];
        assert_eq!(v, [0.0, 0.0, 2.0], "center y (up) lands on local z");
    }
}
