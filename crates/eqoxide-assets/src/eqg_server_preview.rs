//! Opt-in visual counterpart to the server-axis collision adapter.
use crate::{MeshData, ZoneAssets};
use anyhow::Result;
use glam::{Mat4, Vec4};
use std::path::Path;

/// EQG preview geometry prepared for server-axis renderer upload.
/// Numeric adaptation alone does not establish live alignment or gameplay readiness.
/// The wrapped assets retain render-only semantics and contain no collision candidates.
pub struct EqgServerPreview {
    assets: ZoneAssets,
}

impl EqgServerPreview {
    pub fn from_glb(path: &Path) -> Result<Self> {
        Ok(Self::adapt(ZoneAssets::from_preview_glb(path)?))
    }

    pub fn as_assets(&self) -> &ZoneAssets {
        &self.assets
    }

    pub fn into_assets(self) -> ZoneAssets {
        self.assets
    }

    fn adapt(mut assets: ZoneAssets) -> Self {
        // Renderer upload (x,y,z) becomes world (z,x,y). Thus the world X/Y
        // reflection is upload X/Z reflection J, and placements become J M J.
        let reflection = Mat4::from_cols(Vec4::Z, Vec4::Y, Vec4::X, Vec4::W);
        for mesh in &mut assets.terrain {
            reflect_mesh(mesh);
        }
        for object in &mut assets.objects {
            for mesh in &mut object.meshes {
                reflect_mesh(mesh);
            }
            for instance in &mut object.instances {
                *instance = (reflection * Mat4::from_cols_array_2d(instance) * reflection)
                    .to_cols_array_2d();
            }
        }
        Self { assets }
    }
}

fn reflect_mesh(mesh: &mut MeshData) {
    for vector in mesh.positions.iter_mut().chain(mesh.normals.iter_mut()) {
        vector.swap(0, 2);
    }
    mesh.center.swap(0, 2);
    // Reflection changes handedness. Restore the material's original front face.
    for triangle in mesh.indices.chunks_exact_mut(3) {
        triangle.swap(1, 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ObjectModel, RenderMode};
    use glam::{Quat, Vec3};

    fn mesh() -> MeshData {
        MeshData {
            positions: vec![[2., 3., 7.], [6., 3., 7.], [2., 3., 13.]],
            normals: vec![[0., -1., 0.]; 3], indices: vec![0, 1, 2],
            center: [11., 17., 23.], uvs: vec![[0., 0.]; 3],
            vertex_alpha: vec![0.2, 0.6, 0.9], alpha_cutoff: 0.3,
            texture_name: Some("fixture".into()), base_color: [0.1, 0.2, 0.3, 0.4],
            render_mode: RenderMode::Masked, anim: None,
        }
    }

    #[test]
    #[ignore = "requires a locally supplied EQG preview GLB"]
    fn server_preview_native_loader_correspondence() {
        let path = std::env::var_os("EQG_PREVIEW_GLB").expect("set EQG_PREVIEW_GLB");
        let source = ZoneAssets::from_preview_glb(Path::new(&path)).unwrap();
        let server = EqgServerPreview::from_glb(Path::new(&path)).unwrap().into_assets();
        fn check(a: &MeshData, b: &MeshData) {
            assert_eq!(a.positions.len(), b.positions.len());
            for (a, b) in a.positions.iter().zip(&b.positions).chain(a.normals.iter().zip(&b.normals)) {
                assert_eq!(*b, [a[2], a[1], a[0]]);
            }
            assert_eq!(b.center, [a.center[2], a.center[1], a.center[0]]);
            for (a, b) in a.indices.chunks_exact(3).zip(b.indices.chunks_exact(3)) {
                assert_eq!(b, [a[0], a[2], a[1]]);
            }
        }
        assert_eq!(source.terrain.len(), server.terrain.len());
        assert_eq!(source.objects.len(), server.objects.len());
        for (a, b) in source.terrain.iter().zip(&server.terrain) { check(a, b); }
        let mut placements = 0;
        for (a, b) in source.objects.iter().zip(&server.objects) {
            assert_eq!(a.meshes.len(), b.meshes.len());
            assert_eq!(a.instances.len(), b.instances.len());
            for (a, b) in a.meshes.iter().zip(&b.meshes) { check(a, b); }
            for (a, b) in a.instances.iter().zip(&b.instances) {
                // Independent element permutation for J M J (including translation).
                for column in 0..4 { for row in 0..4 {
                    let permutation = [2, 1, 0, 3];
                    assert_eq!(b[column][row], a[permutation[column]][permutation[row]]);
                }}
                placements += 1;
            }
        }
        eprintln!("native visual correspondence: {} terrain meshes, {} object models, {placements} placements", source.terrain.len(), source.objects.len());
    }

    #[test]
    fn server_preview_reflects_terrain_normals_centers_and_winding() {
        let original = mesh();
        let adapted = EqgServerPreview::adapt(ZoneAssets {
            terrain: vec![original.clone()], objects: vec![], textures: vec![],
        });
        let mesh = &adapted.as_assets().terrain[0];
        assert_eq!(mesh.positions, [[7., 3., 2.], [7., 3., 6.], [13., 3., 2.]]);
        assert_eq!(mesh.center, [23., 17., 11.]);
        assert_eq!(mesh.indices, [0, 2, 1]);
        let p = |i: usize| Vec3::from(mesh.positions[mesh.indices[i] as usize]);
        assert!((p(1)-p(0)).cross(p(2)-p(0)).normalize().abs_diff_eq(Vec3::from(mesh.normals[0]), 1e-6));
        assert_eq!(mesh.vertex_alpha, original.vertex_alpha);
        assert_eq!(mesh.alpha_cutoff, original.alpha_cutoff);
        assert_eq!(mesh.base_color, original.base_color);
        assert_eq!(mesh.uvs, original.uvs);
    }

    #[test]
    fn server_preview_conjugates_asymmetric_object_placement_exactly_once() {
        let placement = Mat4::from_scale_rotation_translation(
            Vec3::splat(2.5), Quat::from_euler(glam::EulerRot::ZYX, 0.7, -0.4, 0.2),
            Vec3::new(101., 37., -19.),
        );
        let source = ZoneAssets { terrain: vec![], textures: vec![], objects: vec![ObjectModel {
            name: "asymmetric".into(), meshes: vec![mesh()],
            instances: vec![placement.to_cols_array_2d()],
        }] };
        let before = crate::expand_objects(&source.objects);
        let adapted = EqgServerPreview::adapt(source.clone()).into_assets();
        let after = crate::expand_objects(&adapted.objects);
        for (a, b) in before[0].positions.iter().zip(&after[0].positions) {
            assert!(Vec3::from(*b).abs_diff_eq(Vec3::new(a[2], a[1], a[0]), 1e-4));
        }
        for (a, b) in before[0].normals.iter().zip(&after[0].normals) {
            assert!(Vec3::from(*b).abs_diff_eq(Vec3::new(a[2], a[1], a[0]), 1e-5));
        }
        assert_eq!(after[0].indices, [0, 2, 1]);
        assert_eq!(source.objects[0].instances[0], placement.to_cols_array_2d());
    }
}
