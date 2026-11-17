//! Collision surfaces for the Kelethin lift platforms.
//!
//! The door models come from the zone's doors glb and the heights from
//! [`eqoxide_core::lift::LiftPoses`]. The renderer draws the same models through the same
//! placement, so the drawn platform and the platform a character stands on never differ.

use std::collections::HashMap;

use eqoxide_assets::MeshData;
use eqoxide_core::game_state::Door;
use eqoxide_core::lift::{LiftPoses, LIFT_OPENTYPE};
use eqoxide_zone_geometry::surface::{Surface, SurfaceId, SurfaceKind, SurfaceLayer};

/// A lift door with a smaller `door_param` is a lever, which has no platform to stand on.
const MIN_PLATFORM_RISE: u32 = 10;

/// Returns the meshes of every model in the doors glb at `path`, keyed by uppercase model name,
/// or an empty map if the file is missing or cannot be decoded.
pub fn door_meshes_from_glb(path: &std::path::Path) -> HashMap<String, Vec<MeshData>> {
    match eqoxide_assets::ZoneAssets::object_models_from_glb(path) {
        Ok((models, _textures)) => models,
        Err(e) => {
            tracing::debug!("lift surfaces: no door meshes from {} ({e})", path.display());
            HashMap::new()
        }
    }
}

/// Returns the [`SurfaceLayer`] of every lift platform in `doors` at its pose in `poses`.
///
/// `models` maps an uppercase model name to the meshes of that model. A door without a pose or
/// without a model in `models` contributes no surface. The [`SurfaceId`] of a platform is its
/// door id.
pub fn lift_layer(
    doors: &HashMap<u8, Door>,
    poses: &LiftPoses,
    models: &HashMap<String, Vec<MeshData>>,
) -> SurfaceLayer {
    let mut surfaces: Vec<Surface> = doors.values()
        .filter(|d| d.opentype == LIFT_OPENTYPE && d.door_param >= MIN_PLATFORM_RISE)
        .filter_map(|d| {
            let rise = poses.rise(d.door_id)?;
            let meshes = models.get(&d.name.to_uppercase())?;
            let placement = eqoxide_assets::door_placement([d.x, d.y, d.z], d.heading, d.incline, d.size);
            Some(Surface {
                id: SurfaceId(d.door_id as u32),
                kind: SurfaceKind::Platform,
                tris: eqoxide_assets::door_world_tris(meshes, &placement, rise),
            })
        })
        .collect();
    surfaces.sort_by_key(|s| s.id);
    SurfaceLayer { surfaces }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_assets::RenderMode;

    fn door(id: u8, name: &str, opentype: u8, door_param: u32) -> Door {
        Door {
            door_id: id, name: name.into(), x: 100.0, y: 200.0, z: 10.0, heading: 0.0, incline: 0,
            size: 100, opentype, door_param, invert_state: false, is_open: false,
        }
    }

    /// One glb-space triangle one unit up, so one unit above the door origin in the world.
    fn model() -> HashMap<String, Vec<MeshData>> {
        let mesh = MeshData {
            positions: vec![[0.0, 1.0, 0.0], [0.0, 1.0, 1.0], [1.0, 1.0, 0.0]],
            normals: vec![[0.0, 1.0, 0.0]; 3], uvs: vec![[0.0, 0.0]; 3], indices: vec![0, 1, 2],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        };
        HashMap::from([("FAYLEVATOR".to_string(), vec![mesh])])
    }

    fn posed(doors: &HashMap<u8, Door>) -> LiftPoses {
        let mut p = LiftPoses::default();
        p.tick(doors, 0.0);
        p
    }

    fn doors(list: Vec<Door>) -> HashMap<u8, Door> {
        list.into_iter().map(|d| (d.door_id, d)).collect()
    }

    #[test]
    fn a_platform_becomes_a_surface_at_its_door_id() {
        let d = doors(vec![door(7, "FAYLEVATOR", 59, 68)]);
        let layer = lift_layer(&d, &posed(&d), &model());
        assert_eq!(layer.surfaces.len(), 1);
        assert_eq!(layer.surfaces[0].id, SurfaceId(7));
        assert_eq!(layer.surfaces[0].kind, SurfaceKind::Platform);
        assert_eq!(layer.surfaces[0].tris.len(), 1);
    }

    #[test]
    fn the_surface_follows_the_rise() {
        let mut d = doors(vec![door(7, "FAYLEVATOR", 59, 68)]);
        let mut poses = posed(&d);
        let low = lift_layer(&d, &poses, &model()).surfaces[0].tris[0][0][2];
        d.get_mut(&7).unwrap().is_open = true;
        poses.tick(&d, 1.0);
        let high = lift_layer(&d, &poses, &model()).surfaces[0].tris[0][0][2];
        assert!((high - low - 9.0).abs() < 1e-3, "one second is nine units, got {}", high - low);
    }

    #[test]
    fn a_lever_has_no_surface() {
        let d = doors(vec![door(8, "FAYLEVATOR", 59, 1)]);
        assert!(lift_layer(&d, &posed(&d), &model()).is_empty());
    }

    #[test]
    fn a_door_of_another_opentype_has_no_surface() {
        let d = doors(vec![door(9, "FAYLEVATOR", 5, 68)]);
        assert!(lift_layer(&d, &posed(&d), &model()).is_empty());
    }

    #[test]
    fn a_door_without_a_model_has_no_surface() {
        let d = doors(vec![door(7, "NOSUCHMODEL", 59, 68)]);
        assert!(lift_layer(&d, &posed(&d), &model()).is_empty());
    }

    #[test]
    fn a_door_without_a_pose_has_no_surface() {
        let d = doors(vec![door(7, "FAYLEVATOR", 59, 68)]);
        assert!(lift_layer(&d, &LiftPoses::default(), &model()).is_empty());
    }
}
