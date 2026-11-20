//! Tests for a character riding a ship: the carry moves the rider with the deck, and the controller
//! grounds on the deck the collision layer reports each frame.

use crate::boat_surfaces::{BoatCarry, ShipDeck, ShipPose};
use crate::lift_ride_tests::{flat_zone, idle};
use crate::movement::CharacterController;
use eqoxide_assets::{MeshData, RenderMode};
use eqoxide_renderer::models::ModelBounds;
use eqoxide_zone_geometry::collision::Collision;
use eqoxide_zone_geometry::surface::{SurfaceId, SurfaceLayer};

const DT: f32 = 1.0 / 60.0;

/// A deck 40 units square at height 25, which the ship scale of 1.0 leaves as it is.
fn deck() -> ShipDeck {
    let mesh = MeshData {
        positions: vec![[-20.0, 25.0, -20.0], [20.0, 25.0, -20.0], [20.0, 25.0, 20.0], [-20.0, 25.0, 20.0]],
        normals: vec![[0.0, 1.0, 0.0]; 4], uvs: vec![[0.0, 0.0]; 4],
        indices: vec![0, 1, 2, 0, 2, 3], texture_name: None, base_color: [1.0; 4],
        center: [0.0; 3], render_mode: RenderMode::Opaque, anim: None,
    };
    ShipDeck::from_meshes(&[mesh], ModelBounds { y_bottom: 24.0, y_extent: 100.0, x_center: 0.0, z_center: 0.0 })
}

/// One frame of the loop the app runs: advance the poses, publish the deck, carry the rider by the
/// surface they stood on at the end of the frame before, step, and keep the surface they stand on.
fn frame(
    c: &Collision, deck: &ShipDeck, carry: &mut BoatCarry, ctl: &mut CharacterController,
    rider: &mut Option<SurfaceId>, ship: ShipPose,
) {
    carry.advance(&[ship], DT);
    c.set_surfaces(SurfaceLayer { surfaces: carry.surfaces(deck, &[ship]) });
    ctl.pos = carry.carry(*rider, ctl.pos);
    ctl.step(idle(), DT, c);
    *rider = ctl.standing_on(c);
}

fn ship(pos: [f32; 3], heading: f32) -> ShipPose {
    ShipPose { spawn_id: 7, pos, heading }
}

#[test]
fn a_rider_on_the_deck_sails_with_the_ship() {
    let c = flat_zone();
    let (deck, mut carry, mut rider) = (deck(), BoatCarry::default(), None);
    let mut ctl = CharacterController::new([0.0, -80.0, 25.0]);
    for _ in 0..30 { frame(&c, &deck, &mut carry, &mut ctl, &mut rider, ship([0.0, -80.0, 0.0], 0.0)); }
    assert!(ctl.on_ground && (ctl.pos[2] - 25.0).abs() < 0.05, "setup: {:?}", ctl.pos);

    // ten seconds due north at 10 units per second
    for n in 1..=600 {
        let at = ship([0.0, -80.0 + 10.0 * DT * n as f32, 0.0], 0.0);
        frame(&c, &deck, &mut carry, &mut ctl, &mut rider, at);
        assert!(ctl.on_ground, "left the deck on frame {n}");
        assert!((ctl.pos[1] - at.pos[1]).abs() < 0.2, "frame {n}: rider {} ship {}", ctl.pos[1], at.pos[1]);
    }
    assert_eq!(ctl.standing_on(&c), Some(ship([0.0; 3], 0.0).surface_id()));
}

#[test]
fn a_rider_off_the_deck_is_left_behind() {
    let c = flat_zone();
    let (deck, mut carry, mut rider) = (deck(), BoatCarry::default(), None);
    // 30 units east of the ship, past the 20 unit deck edge, on the floor
    let mut ctl = CharacterController::new([30.0, -80.0, 0.0]);
    for _ in 0..30 { frame(&c, &deck, &mut carry, &mut ctl, &mut rider, ship([0.0, -80.0, 0.0], 0.0)); }
    for n in 1..=300 {
        frame(&c, &deck, &mut carry, &mut ctl, &mut rider, ship([0.0, -80.0 + 10.0 * DT * n as f32, 0.0], 0.0));
    }
    assert!((ctl.pos[0] - 30.0).abs() < 0.01 && (ctl.pos[1] + 80.0).abs() < 0.01, "{:?}", ctl.pos);
}

#[test]
fn a_rider_turns_with_the_ship() {
    let c = flat_zone();
    let (deck, mut carry, mut rider) = (deck(), BoatCarry::default(), None);
    // 10 units east of the ship's origin, which is on the deck
    let mut ctl = CharacterController::new([10.0, -50.0, 25.0]);
    for _ in 0..30 { frame(&c, &deck, &mut carry, &mut ctl, &mut rider, ship([0.0, -50.0, 0.0], 0.0)); }
    // a quarter turn counterclockwise over three seconds
    for n in 1..=180 {
        frame(&c, &deck, &mut carry, &mut ctl, &mut rider, ship([0.0, -50.0, 0.0], 90.0 * n as f32 / 180.0));
    }
    assert!(ctl.pos[0].abs() < 0.05 && (ctl.pos[1] - (-40.0)).abs() < 0.05, "{:?}", ctl.pos);
}
