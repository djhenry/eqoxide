//! Synthetic zone geometry for `--testzone` (offline rendering with no EQ server): a brown ground
//! plane + colored XYZ axis sticks, for verifying the coordinate transform and model placement.

// Synthetic zone geometry for "testzone" — brown ground plane + XYZ axis sticks.
//
// COORDINATE NOTE: upload_zone_assets converts [X, Y, Z] → GPU [X, Z, Y].
// This matches S3D native format: X=east, Y=height(up), Z=north.
// So we author all vertices here in S3D format:
//   S3D X → EQ X (east)
//   S3D Y → EQ Z (height/up)
//   S3D Z → EQ Y (north)

use crate::assets::{MeshData, TextureData, ZoneAssets};

pub fn make_debug_zone() -> ZoneAssets {
    let textures = vec![
        grass_tex("grass"),
        solid_tex("red",   [220, 40,  40, 255]),  // EQ +X east
        solid_tex("green", [ 40, 200, 40, 255]),  // EQ +Y north
        solid_tex("blue",  [ 40,  80, 220, 255]), // EQ +Z up
    ];
    let meshes = vec![
        ground_plane("grass"),
        // EQ +X east = S3D X direction: tip at S3D (20,0,0)
        axis_box([ 0.0,20.0], [-0.5, 0.5], [-0.5, 0.5], "red"),
        // EQ +Y north = S3D Z direction: tip at S3D (0,0,20)
        axis_box([-0.5, 0.5], [-0.5, 0.5], [ 0.0,20.0], "green"),
        // EQ +Z up = S3D Y direction: tip at S3D (0,20,0)
        axis_box([-0.5, 0.5], [ 0.0,20.0], [-0.5, 0.5], "blue"),
    ];
    ZoneAssets { terrain: meshes, objects: vec![], textures }
}

/// Fallback environment used when a zone's S3D file is missing.
/// Just a large grass ground plane with no axis markers.
pub fn make_fallback_ground() -> ZoneAssets {
    let textures = vec![grass_tex("grass")];
    let meshes = vec![ground_plane("grass")];
    ZoneAssets { terrain: meshes, objects: vec![], textures }
}

fn solid_tex(name: &str, rgba: [u8; 4]) -> TextureData {
    TextureData { name: name.to_string(), width: 1, height: 1, rgba: rgba.to_vec() }
}

/// 2×2 checkerboard grass texture — two shades of green give visual scale.
fn grass_tex(name: &str) -> TextureData {
    let dark  = [42u8,  90, 35, 255];  // dark grass
    let light = [65u8, 120, 50, 255];  // light grass
    let rgba = [dark, light, light, dark].into_iter().flatten().collect();
    TextureData { name: name.to_string(), width: 2, height: 2, rgba }
}

/// Large flat quad at S3D Y=0 (EQ ground level). Tiling UVs for visual scale.
fn ground_plane(tex: &str) -> MeshData {
    let s = 1500.0_f32;
    let t = s / 20.0;  // UV scale — each tile ≈ 20 EQ units wide
    // S3D: X=east, Y=height=0, Z=north
    let positions = vec![
        [-s, 0.0, -s], [s, 0.0, -s], [s, 0.0, s], [-s, 0.0, s],
    ];
    let normals = vec![[0.0, 1.0, 0.0]; 4];
    let uvs     = vec![[0.0, 0.0], [t, 0.0], [t, t], [0.0, t]];
    let indices = vec![0u32, 1, 2, 0, 2, 3];
    MeshData { positions, normals, uvs, indices,
               texture_name: Some(tex.into()), base_color: [1.0; 4],
               center: [0.0; 3], render_mode: crate::assets::RenderMode::Opaque, anim: None }
}

/// Box axis stick in S3D coordinates. x_range, y_range, z_range each [min, max].
fn axis_box(x: [f32; 2], y: [f32; 2], z: [f32; 2], tex: &str) -> MeshData {
    let [x0, x1] = x;
    let [y0, y1] = y;
    let [z0, z1] = z;

    let positions = vec![
        [x0,y0,z0],[x1,y0,z0],[x1,y1,z0],[x0,y1,z0], // -Z face
        [x0,y0,z1],[x1,y0,z1],[x1,y1,z1],[x0,y1,z1], // +Z face
    ];
    let normals = vec![[0.0, 1.0, 0.0]; 8]; // approximate
    let uvs     = vec![[0.0, 0.0]; 8];
    #[rustfmt::skip]
    let indices: Vec<u32> = vec![
        0,2,1, 0,3,2,  // -Z
        4,5,6, 4,6,7,  // +Z
        0,1,5, 0,5,4,  // -Y
        2,3,7, 2,7,6,  // +Y
        0,4,7, 0,7,3,  // -X
        1,2,6, 1,6,5,  // +X
    ];
    MeshData { positions, normals, uvs, indices,
               texture_name: Some(tex.into()), base_color: [1.0; 4],
               center: [0.0; 3], render_mode: crate::assets::RenderMode::Opaque, anim: None }
}
