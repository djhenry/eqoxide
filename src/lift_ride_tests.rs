//! A character riding a moving platform: the controller grounds on the platform the collision
//! layer reports each frame, with no carry applied to the rider.

use crate::movement::{CharacterController, MoveIntent};
use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
use eqoxide_core::lift::LIFT_SPEED;
use eqoxide_zone_geometry::collision::Collision;
use eqoxide_zone_geometry::surface::{Surface, SurfaceId, SurfaceKind, SurfaceLayer};

const PLATFORM: SurfaceId = SurfaceId(1);

/// A zone with a floor at z 0 over east and north from -100 to 100.
fn flat_zone() -> Collision {
    let floor = MeshData {
        positions: vec![[-100.0, 0.0, -100.0], [100.0, 0.0, -100.0], [100.0, 0.0, 100.0], [-100.0, 0.0, 100.0]],
        normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
        indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
        center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
    };
    Collision::build(&ZoneAssets { terrain: vec![floor], objects: vec![], textures: vec![] }, 8.0)
}

/// Publishes a 20 by 20 up-facing platform centered on the origin at height `z`.
fn set_platform(c: &Collision, z: f32) {
    let (a, b, d, e) = ([-10.0, -10.0, z], [10.0, -10.0, z], [10.0, 10.0, z], [-10.0, 10.0, z]);
    c.set_surfaces(SurfaceLayer {
        surfaces: vec![Surface { id: PLATFORM, kind: SurfaceKind::Platform, tris: vec![[a, b, d], [a, d, e]], velocity: [0.0; 3] }],
    });
}

fn idle() -> MoveIntent {
    MoveIntent { wish_dir: [0.0, 0.0], wish_vspeed: 0.0, jump: false, want_swim: false, want_climb: false,
                 speed: 0.0, hop: false }
}

fn walk_east(speed: f32) -> MoveIntent {
    MoveIntent { wish_dir: [1.0, 0.0], wish_vspeed: 0.0, jump: false, want_swim: false, want_climb: false,
                 speed, hop: false }
}

/// Returns a controller standing on a platform at height `z`.
fn standing_at(c: &Collision, z: f32) -> CharacterController {
    set_platform(c, z);
    let mut ctl = CharacterController::new([0.0, 0.0, z]);
    for _ in 0..30 { ctl.step(idle(), 1.0 / 60.0, c); }
    assert!(ctl.on_ground && (ctl.pos[2] - z).abs() < 0.01, "setup: {:?}", ctl.pos);
    ctl
}

#[test]
fn a_rising_platform_carries_the_rider_up() {
    let c = flat_zone();
    let mut ctl = standing_at(&c, 0.0);
    let dt = 1.0 / 60.0;
    let mut z = 0.0;
    for frame in 0..(7 * 60) {
        z += LIFT_SPEED * dt;
        set_platform(&c, z);
        ctl.step(idle(), dt, &c);
        assert!(ctl.on_ground, "left the platform on frame {frame} at z {z}");
        assert!((ctl.pos[2] - z).abs() < 0.01, "frame {frame}: rider {} platform {z}", ctl.pos[2]);
        assert_eq!(ctl.standing_on(&c), Some(PLATFORM));
    }
}

#[test]
fn a_descending_platform_carries_the_rider_down() {
    for dt in [1.0 / 60.0, 1.0 / 30.0, 1.0 / 20.0] {
        let c = flat_zone();
        let mut ctl = standing_at(&c, 68.0);
        let mut z = 68.0;
        let mut frame = 0;
        while z > 0.0 {
            z = (z - LIFT_SPEED * dt).max(0.0);
            set_platform(&c, z);
            ctl.step(idle(), dt, &c);
            assert!(ctl.on_ground, "dt {dt}: left the platform on frame {frame} at z {z}");
            assert!((ctl.pos[2] - z).abs() < 0.01, "dt {dt} frame {frame}: rider {} platform {z}", ctl.pos[2]);
            frame += 1;
        }
    }
}

#[test]
fn walking_off_the_edge_drops_to_the_ground() {
    let c = flat_zone();
    let mut ctl = standing_at(&c, 20.0);
    for _ in 0..(5 * 60) { ctl.step(walk_east(14.0), 1.0 / 60.0, &c); }
    assert!(ctl.pos[0] > 10.0, "walked off the east edge, at {:?}", ctl.pos);
    assert!(ctl.on_ground && ctl.pos[2].abs() < 0.01, "landed on the floor at z 0, at {:?}", ctl.pos);
    assert_eq!(ctl.standing_on(&c), None);
}

#[test]
fn a_jump_from_the_platform_leaves_it() {
    let c = flat_zone();
    let mut ctl = standing_at(&c, 20.0);
    let mut jump = idle();
    jump.jump = true;
    ctl.step(jump, 1.0 / 60.0, &c);
    assert!(!ctl.on_ground);
    assert_eq!(ctl.standing_on(&c), None, "a character in the air is not standing on the platform");
}

#[test]
fn a_character_on_static_ground_stands_on_no_surface() {
    let c = flat_zone();
    let mut ctl = CharacterController::new([50.0, 0.0, 0.0]);
    for _ in 0..30 { ctl.step(idle(), 1.0 / 60.0, &c); }
    assert_eq!(ctl.standing_on(&c), None);
}
