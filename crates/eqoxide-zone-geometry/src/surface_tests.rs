//! Tests for the moving-surface layer inside [`Collision`].

use crate::collision::{ClearanceField, Collision};
use crate::surface::{Surface, SurfaceId, SurfaceKind, SurfaceLayer};
use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};

/// A zone with one floor at z 0 covering east and north from -100 to 100.
fn flat_zone() -> Collision {
    let floor = MeshData {
        positions: vec![[-100.0, 0.0, -100.0], [100.0, 0.0, -100.0], [100.0, 0.0, 100.0], [-100.0, 0.0, 100.0]],
        normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
        indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
        center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
    };
    Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 8.0)
}

/// An up-facing 10 by 10 square centered on the origin at height `z`.
fn platform(id: u32, z: f32) -> Surface {
    let (a, b, c, d) = ([-5.0, -5.0, z], [5.0, -5.0, z], [5.0, 5.0, z], [-5.0, 5.0, z]);
    Surface { id: SurfaceId(id), kind: SurfaceKind::Platform, tris: vec![[a, b, c], [a, c, d]] }
}

fn layer(surfaces: Vec<Surface>) -> SurfaceLayer {
    SurfaceLayer { surfaces }
}

#[test]
fn platform_raises_nearest_floor() {
    let c = flat_zone();
    c.set_surfaces(layer(vec![platform(7, 5.0)]));
    assert_eq!(c.nearest_floor(0.0, 0.0, 6.0, 3.0, 3.0), Some(5.0));
    assert_eq!(c.nearest_floor(50.0, 0.0, 6.0, 3.0, 3.0), None, "off the platform the floor at z 0 is out of reach");
}

#[test]
fn surface_under_reports_the_id() {
    let c = flat_zone();
    c.set_surfaces(layer(vec![platform(7, 5.0)]));
    assert_eq!(c.surface_under(0.0, 0.0, 5.0, 3.0, 3.0), Some(SurfaceId(7)));
    assert_eq!(c.surface_under(50.0, 0.0, 0.0, 3.0, 3.0), None, "static ground is not a surface");
}

#[test]
fn surface_under_ignores_a_platform_the_character_is_not_on() {
    let c = flat_zone();
    c.set_surfaces(layer(vec![platform(7, 5.0)]));
    assert_eq!(c.surface_under(0.0, 0.0, 0.0, 3.0, 3.0), None, "the floor within reach at z 0 is the static one");
}

#[test]
fn platform_moves_with_the_layer() {
    let c = flat_zone();
    let ray = |c: &Collision| c.nearest_hit_t([0.0, 0.0, 30.0], [0.0, 0.0, 10.0]);
    c.set_surfaces(layer(vec![platform(7, 5.0)]));
    assert_eq!(ray(&c), None);
    c.set_surfaces(layer(vec![platform(7, 20.0)]));
    let t = ray(&c).expect("a platform at z 20 crosses the ray");
    assert!((t - 0.5).abs() < 1e-3, "hit at t={t}");
    c.set_surfaces(layer(vec![platform(7, 40.0)]));
    assert_eq!(ray(&c), None);
}

#[test]
fn empty_layer_changes_nothing() {
    let c = flat_zone();
    let before = (c.nearest_floor(0.0, 0.0, 1.0, 3.0, 3.0), c.nearest_hit_t([0.0, 0.0, 5.0], [0.0, 0.0, -5.0]));
    c.set_surfaces(layer(vec![platform(7, 5.0)]));
    c.set_surfaces(SurfaceLayer::default());
    let after = (c.nearest_floor(0.0, 0.0, 1.0, 3.0, 3.0), c.nearest_hit_t([0.0, 0.0, 5.0], [0.0, 0.0, -5.0]));
    assert_eq!(before, after);
}

#[test]
fn clearance_field_ignores_surfaces() {
    let c = flat_zone();
    let field = ClearanceField::default();
    let free = field.wall_at(&c, 0.0, 0.0, 0.0);
    // a vertical wall at east 3, from z -5 to 50, that a horizontal ray at the probe heights meets
    let (a, b, d, e) = ([3.0, -5.0, -5.0], [3.0, 5.0, -5.0], [3.0, 5.0, 50.0], [3.0, -5.0, 50.0]);
    c.set_surfaces(layer(vec![Surface { id: SurfaceId(9), kind: SurfaceKind::Platform, tris: vec![[a, b, d], [a, d, e]] }]));
    assert!(c.nearest_hit_t([0.0, 0.0, 2.0], [10.0, 0.0, 2.0]).is_some(), "the query sees the wall");
    let fresh = ClearanceField::default();
    assert_eq!(fresh.wall_at(&c, 0.0, 0.0, 0.0), free, "the memoised clearance must not see a moving wall");
}
