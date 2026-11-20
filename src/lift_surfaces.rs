//! Collision surfaces for the Kelethin lift platforms, and the one publisher that sets every moving
//! surface of the zone, lifts and ship decks together, on the collision.
//!
//! The door models come from the zone's doors glb and the heights from
//! [`eqoxide_core::lift::LiftPoses`]. The renderer draws the same models through the same
//! placement, so the drawn platform and the platform a character stands on never differ.

use std::collections::HashMap;

use eqoxide_assets::MeshData;
use eqoxide_core::game_state::Door;
use eqoxide_core::lift::{LiftPoses, LIFT_OPENTYPE, LIFT_SPEED};
use eqoxide_zone_geometry::collision::Collision;
use eqoxide_zone_geometry::surface::{Surface, SurfaceId, SurfaceKind, SurfaceLayer};

/// Publishes the lift platforms and the ship decks to a [`Collision`], rebuilding the surfaces only
/// when a platform or a deck moved or a lift door changed. [`Collision::set_surfaces`] replaces the
/// whole layer, so both kinds go through this one publisher.
#[derive(Default)]
pub struct SurfacePublisher {
    published: Option<(Vec<LiftKey>, Vec<Surface>)>,
}

/// Everything about a lift door that the surfaces of [`lift_layer`] depend on, besides the models.
type LiftKey = (u8, u32, bool, [u32; 5], u16);

impl SurfacePublisher {
    /// Sets the surfaces of `collision` to the lift platforms of `doors` at their `poses` and the
    /// ship decks `boats`, unless they equal the last published surfaces.
    pub fn publish(
        &mut self,
        collision: &Collision,
        doors: &HashMap<u8, Door>,
        poses: &LiftPoses,
        models: &HashMap<String, Vec<MeshData>>,
        boats: Vec<Surface>,
    ) {
        let mut key: Vec<LiftKey> = doors.values()
            .filter(|d| d.opentype == LIFT_OPENTYPE)
            .map(|d| (
                d.door_id, poses.rise(d.door_id).map_or(u32::MAX, f32::to_bits), d.is_open,
                [d.x.to_bits(), d.y.to_bits(), d.z.to_bits(), d.heading.to_bits(), d.door_param],
                d.size,
            ))
            .collect();
        key.sort_unstable();
        if self.published.as_ref().is_some_and(|(k, b)| *k == key && *b == boats) { return; }
        let mut layer = lift_layer(doors, poses, models);
        layer.surfaces.extend(boats.iter().cloned());
        collision.set_surfaces(layer);
        self.published = Some((key, boats));
    }

    /// Forgets the published surfaces, so the next [`SurfacePublisher::publish`] rebuilds them. Call
    /// it when the [`Collision`] or the door models change.
    pub fn invalidate(&mut self) {
        self.published = None;
    }
}

/// A lift door with a smaller `door_param` is a lever, which has no platform to stand on.
const MIN_PLATFORM_RISE: u32 = 10;

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
            let placement = eqoxide_assets::lift_placement([d.x, d.y, d.z], d.heading, d.size);
            let target = if d.is_open { d.door_param as f32 } else { 0.0 };
            let vz = match rise - target {
                gap if gap.abs() < 1e-3 => 0.0,
                gap if gap < 0.0 => LIFT_SPEED,
                _ => -LIFT_SPEED,
            };
            Some(Surface {
                id: SurfaceId(d.door_id as u32),
                kind: SurfaceKind::Platform,
                tris: eqoxide_assets::door_world_tris(meshes, &placement, rise),
                velocity: [0.0, 0.0, vz],
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
    fn velocity_is_the_lift_speed_while_travelling_and_zero_at_rest() {
        let mut d = doors(vec![door(7, "FAYLEVATOR", 59, 68)]);
        let mut poses = posed(&d);
        assert_eq!(lift_layer(&d, &poses, &model()).surfaces[0].velocity, [0.0; 3], "closed and at rest");
        d.get_mut(&7).unwrap().is_open = true;
        poses.tick(&d, 1.0);
        assert_eq!(lift_layer(&d, &poses, &model()).surfaces[0].velocity, [0.0, 0.0, 9.0], "rising");
        poses.tick(&d, 20.0);
        assert_eq!(lift_layer(&d, &poses, &model()).surfaces[0].velocity, [0.0; 3], "arrived at the top");
        d.get_mut(&7).unwrap().is_open = false;
        poses.tick(&d, 1.0);
        assert_eq!(lift_layer(&d, &poses, &model()).surfaces[0].velocity, [0.0, 0.0, -9.0], "falling");
    }

    #[test]
    fn the_publisher_rebuilds_only_when_a_platform_moved() {
        let mut d = doors(vec![door(7, "FAYLEVATOR", 59, 68)]);
        let mut poses = posed(&d);
        let c = Collision::build(&eqoxide_assets::ZoneAssets { terrain: vec![], objects: vec![], textures: vec![] }, 8.0);
        let mut p = SurfacePublisher::default();
        p.publish(&c, &d, &poses, &model(), vec![]);
        let first = c.surface_layer();
        p.publish(&c, &d, &poses, &model(), vec![]);
        assert!(std::sync::Arc::ptr_eq(&first, &c.surface_layer()), "nothing moved");
        d.get_mut(&7).unwrap().is_open = true;
        poses.tick(&d, 1.0);
        p.publish(&c, &d, &poses, &model(), vec![]);
        assert!(!std::sync::Arc::ptr_eq(&first, &c.surface_layer()), "the platform rose");
        let second = c.surface_layer();
        p.invalidate();
        p.publish(&c, &d, &poses, &model(), vec![]);
        assert!(!std::sync::Arc::ptr_eq(&second, &c.surface_layer()), "invalidated");
    }

    #[test]
    fn the_publisher_sets_lifts_and_boats_together_and_rebuilds_when_a_boat_moves() {
        let d = doors(vec![door(7, "FAYLEVATOR", 59, 68)]);
        let poses = posed(&d);
        let c = Collision::build(&eqoxide_assets::ZoneAssets { terrain: vec![], objects: vec![], textures: vec![] }, 8.0);
        let boat = |east: f32| Surface {
            id: SurfaceId(0x1_0007), kind: SurfaceKind::Boat,
            tris: vec![[[east, 0.0, 0.0], [east, 1.0, 0.0], [east + 1.0, 0.0, 0.0]]], velocity: [0.0; 3],
        };
        let mut p = SurfacePublisher::default();
        p.publish(&c, &d, &poses, &model(), vec![boat(0.0)]);
        let ids: Vec<SurfaceId> = c.surface_layer().surfaces.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![SurfaceId(7), SurfaceId(0x1_0007)], "the lift and the boat");
        let first = c.surface_layer();
        p.publish(&c, &d, &poses, &model(), vec![boat(0.0)]);
        assert!(std::sync::Arc::ptr_eq(&first, &c.surface_layer()), "the boat did not move");
        p.publish(&c, &d, &poses, &model(), vec![boat(5.0)]);
        assert!(!std::sync::Arc::ptr_eq(&first, &c.surface_layer()), "the boat sailed");
        p.publish(&c, &d, &poses, &model(), vec![]);
        assert_eq!(c.surface_layer().surfaces.len(), 1, "the boat left");
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
