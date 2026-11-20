//! Collision surfaces for the decks of ships such as the Freeport ferry, and the carry that moves a
//! rider with the ship under their feet.
//!
//! The deck triangles come from the same glb the renderer draws and are placed with the renderer's
//! own matrix, so the drawn deck and the deck a character stands on never differ.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use eqoxide_assets::MeshData;
use eqoxide_renderer::camera::entity_model_matrix_static;
use eqoxide_renderer::models::{archetype_correction, static_placement, ModelAsset, ModelBounds};
use eqoxide_zone_geometry::surface::{Surface, SurfaceId, SurfaceKind};

/// The archetype of the ship model, whose glb the deck triangles come from.
pub const SHIP_ARCHETYPE: &str = "ship";

/// A vertex normal this far up (`y` in the glb's Y-up space) makes a triangle a floor.
const MIN_UP_NORMAL: f32 = 0.7;

/// Model heights of the walkable decks: below [`MIN_DECK_Y`] is the hull interior and above
/// [`MAX_DECK_Y`] are the crow's nests and yards, which the native collision hull leaves out.
const MIN_DECK_Y: f32 = 20.0;
const MAX_DECK_Y: f32 = 60.0;

/// A deck triangle is flat: its corners are within this many model units of one height, so a ramp
/// or a rail that merely faces up does not count as floor.
const MAX_PLANK_SLOPE_Y: f32 = 1.0;

/// A ship that moves farther than this in one frame was moved by the server, not by sailing, so
/// its riders are not carried along.
const MAX_CARRY_SHIFT: f32 = 50.0;

/// The [`SurfaceId`] of a ship deck is this plus the spawn id of the ship, which keeps it apart from
/// the door ids of the lift platforms.
const BOAT_ID_BASE: u32 = 0x1_0000;

/// The pose of one ship this frame, in client coordinates `[east, north, up]` and EQ heading
/// degrees.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShipPose {
    pub spawn_id: u32,
    pub pos: [f32; 3],
    pub heading: f32,
}

impl ShipPose {
    /// Returns the id of the deck surface of this ship.
    pub fn surface_id(&self) -> SurfaceId {
        SurfaceId(BOAT_ID_BASE + self.spawn_id)
    }
}

/// Returns the pose of every ship among `billboards`: the entities drawn with the ship model.
pub fn ship_poses(billboards: &[eqoxide_renderer::scene::Billboard]) -> Vec<ShipPose> {
    billboards.iter()
        .filter(|b| eqoxide_renderer::models::race_to_archetype(&b.race) == SHIP_ARCHETYPE)
        .map(|b| ShipPose { spawn_id: b.id, pos: b.pos, heading: b.heading })
        .collect()
}

/// The walkable triangles of a ship model, in the glb's own space, with the bounds the renderer
/// places that model by.
pub struct ShipDeck {
    tris: Vec<[[f32; 3]; 3]>,
    bounds: ModelBounds,
}

impl ShipDeck {
    /// Returns the deck of the ship model in the glb at `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let asset = ModelAsset::load(path)?;
        let bounds = ModelBounds {
            y_bottom: asset.y_bottom, y_extent: asset.y_extent,
            x_center: asset.x_center, z_center: asset.z_center,
        };
        Ok(Self::from_meshes(&asset.meshes, bounds))
    }

    /// Returns the deck of `meshes`: the upward-facing triangles between [`MIN_DECK_Y`] and
    /// [`MAX_DECK_Y`].
    pub fn from_meshes(meshes: &[MeshData], bounds: ModelBounds) -> Self {
        let mut tris = Vec::new();
        for mesh in meshes {
            for idx in mesh.indices.chunks_exact(3) {
                let corners = |src: &[[f32; 3]]| -> Option<[[f32; 3]; 3]> {
                    Some([*src.get(idx[0] as usize)?, *src.get(idx[1] as usize)?, *src.get(idx[2] as usize)?])
                };
                let (Some(v), Some(n)) = (corners(&mesh.positions), corners(&mesh.normals)) else { continue };
                let up = (n[0][1] + n[1][1] + n[2][1]) / 3.0;
                let y = (v[0][1] + v[1][1] + v[2][1]) / 3.0;
                let flat = v.iter().all(|p| (p[1] - y).abs() <= MAX_PLANK_SLOPE_Y);
                if flat && up >= MIN_UP_NORMAL && (MIN_DECK_Y..=MAX_DECK_Y).contains(&y) {
                    tris.push(v);
                }
            }
        }
        Self { tris, bounds }
    }

    /// Returns the number of deck triangles.
    pub fn len(&self) -> usize {
        self.tris.len()
    }

    /// Returns the matrix that places the glb at `pose`, the one the renderer draws the ship with.
    fn matrix(&self, pose: &ShipPose) -> glam::Mat4 {
        let placement = static_placement(SHIP_ARCHETYPE, &self.bounds, true);
        glam::Mat4::from_cols_array_2d(&entity_model_matrix_static(
            pose.pos, pose.heading, &placement, archetype_correction(SHIP_ARCHETYPE)))
    }

    /// Returns the deck of one ship at `pose` as a surface moving at `velocity`.
    fn surface(&self, pose: &ShipPose, velocity: [f32; 3]) -> Surface {
        let m = self.matrix(pose);
        let place = |p: [f32; 3]| m.transform_point3(glam::Vec3::from(p)).to_array();
        Surface {
            id: pose.surface_id(),
            kind: SurfaceKind::Boat,
            tris: self.tris.iter().map(|t| [place(t[0]), place(t[1]), place(t[2])]).collect(),
            velocity,
        }
    }
}

/// Tracks the poses of the ships over frames, for the velocity of each deck and for the carry of
/// a rider.
#[derive(Default)]
pub struct BoatCarry {
    before: HashMap<SurfaceId, ShipPose>,
    now: HashMap<SurfaceId, ShipPose>,
    velocity: HashMap<SurfaceId, [f32; 3]>,
}

impl BoatCarry {
    /// Records the poses of this frame, keeping those of the frame before.
    pub fn advance(&mut self, ships: &[ShipPose], dt: f32) {
        self.before = std::mem::take(&mut self.now);
        self.now = ships.iter().map(|s| (s.surface_id(), *s)).collect();
        self.velocity = self.now.iter()
            .filter_map(|(id, cur)| {
                let prev = self.before.get(id)?;
                if dt <= 0.0 { return Some((*id, [0.0; 3])); }
                Some((*id, [
                    (cur.pos[0] - prev.pos[0]) / dt,
                    (cur.pos[1] - prev.pos[1]) / dt,
                    (cur.pos[2] - prev.pos[2]) / dt,
                ]))
            })
            .collect();
    }

    /// Returns the decks of `ships` as surfaces at the poses of the last [`BoatCarry::advance`].
    pub fn surfaces(&self, deck: &ShipDeck, ships: &[ShipPose]) -> Vec<Surface> {
        let mut out: Vec<Surface> = ships.iter()
            .map(|s| deck.surface(s, self.velocity.get(&s.surface_id()).copied().unwrap_or([0.0; 3])))
            .collect();
        out.sort_by_key(|s| s.id);
        out
    }

    /// Returns where a rider at `rider` ends up when the ship they stand on, `standing_on`, moved
    /// from its previous pose to its current one: it shifts by the ship's translation and swings
    /// around the ship's origin by the ship's turn. The height is left to the floor probe.
    pub fn carry(&self, standing_on: Option<SurfaceId>, rider: [f32; 3]) -> [f32; 3] {
        let Some(id) = standing_on else { return rider };
        let (Some(prev), Some(cur)) = (self.before.get(&id), self.now.get(&id)) else { return rider };
        let shift = (cur.pos[0] - prev.pos[0]).hypot(cur.pos[1] - prev.pos[1]);
        if shift > MAX_CARRY_SHIFT { return rider; }
        let turn = (cur.heading - prev.heading).to_radians();
        let (sin, cos) = turn.sin_cos();
        let (dx, dy) = (rider[0] - prev.pos[0], rider[1] - prev.pos[1]);
        [
            cur.pos[0] + dx * cos - dy * sin,
            cur.pos[1] + dx * sin + dy * cos,
            rider[2],
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_assets::RenderMode;

    fn bounds() -> ModelBounds {
        ModelBounds { y_bottom: 24.0, y_extent: 100.0, x_center: 0.0, z_center: 0.0 }
    }

    /// A one-triangle mesh at model height `y` whose normals point `ny` in y.
    fn tri_at(y: f32, ny: f32) -> MeshData {
        MeshData {
            positions: vec![[10.0, y, 4.0], [10.0, y, 5.0], [11.0, y, 4.0]],
            normals: vec![[0.0, ny, 0.0]; 3], uvs: vec![[0.0, 0.0]; 3], indices: vec![0, 1, 2],
            texture_name: None, base_color: [1.0; 4], center: [0.0; 3],
            render_mode: RenderMode::Opaque, anim: None,
        }
    }

    fn deck() -> ShipDeck {
        ShipDeck::from_meshes(&[tri_at(25.0, 1.0)], bounds())
    }

    fn billboard(id: u32, race: &str, pos: [f32; 3], heading: f32) -> eqoxide_renderer::scene::Billboard {
        eqoxide_renderer::scene::Billboard {
            id, pos, level: 1, hp_pct: 100.0, is_target: false, dead: false,
            name: format!("npc{id}"), race: race.into(), action: "idle".into(), heading,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, face: 0, hairstyle: 0,
            haircolor: 0, helm: 0, showhelm: 0, floating: true, gait: None, untargetable: false,
        }
    }

    #[test]
    fn only_ships_have_a_deck() {
        let all = [
            billboard(1, "SHP", [1.0, 2.0, 3.0], 90.0),  // the ferry
            billboard(2, "BOT", [0.0; 3], 0.0),          // a rowboat
            billboard(3, "HUM", [0.0; 3], 0.0),
        ];
        assert_eq!(ship_poses(&all), vec![pose(1, [1.0, 2.0, 3.0], 90.0)]);
    }

    fn pose(id: u32, pos: [f32; 3], heading: f32) -> ShipPose {
        ShipPose { spawn_id: id, pos, heading }
    }

    #[test]
    fn the_deck_keeps_only_upward_triangles_between_the_hull_and_the_rigging() {
        let meshes = [
            tri_at(25.0, 1.0),   // main deck
            tri_at(25.0, -1.0),  // its underside
            tri_at(5.0, 1.0),    // hull interior
            tri_at(91.0, 1.0),   // crow's nest
        ];
        assert_eq!(ShipDeck::from_meshes(&meshes, bounds()).len(), 1);
    }

    #[test]
    fn a_deck_triangle_is_placed_where_the_renderer_draws_the_model() {
        // The model-to-world mapping at heading 0: model x runs north, model z runs east, model y
        // runs up, at the ship scale and about the ship's position.
        let s = deck().surface(&pose(7, [100.0, 200.0, -5.0], 0.0), [0.0; 3]);
        let scale = eqoxide_renderer::models::archetype_scale(SHIP_ARCHETYPE);
        let v = s.tris[0][0];
        assert!((v[0] - (100.0 + 4.0 * scale)).abs() < 1e-3, "east {v:?}");
        assert!((v[1] - (200.0 + 10.0 * scale)).abs() < 1e-3, "north {v:?}");
        assert!((v[2] - (-5.0 + 25.0 * scale)).abs() < 1e-3, "up {v:?}");
        assert_eq!(s.id, SurfaceId(BOAT_ID_BASE + 7));
        assert_eq!(s.kind, SurfaceKind::Boat);
    }

    #[test]
    fn the_deck_follows_the_ship() {
        let d = deck();
        let a = d.surface(&pose(7, [0.0, 0.0, 0.0], 0.0), [0.0; 3]).tris[0][0];
        let b = d.surface(&pose(7, [30.0, 40.0, 0.0], 0.0), [0.0; 3]).tris[0][0];
        assert!((b[0] - a[0] - 30.0).abs() < 1e-3 && (b[1] - a[1] - 40.0).abs() < 1e-3);
    }

    #[test]
    fn velocity_is_the_pose_change_over_the_frame() {
        let mut c = BoatCarry::default();
        c.advance(&[pose(7, [0.0, 0.0, 0.0], 0.0)], 0.1);
        c.advance(&[pose(7, [1.0, 2.0, 0.0], 0.0)], 0.5);
        let s = c.surfaces(&deck(), &[pose(7, [1.0, 2.0, 0.0], 0.0)]);
        assert_eq!(s[0].velocity, [2.0, 4.0, 0.0]);
    }

    #[test]
    fn a_ship_seen_for_the_first_time_is_at_rest() {
        let mut c = BoatCarry::default();
        c.advance(&[pose(7, [5.0, 5.0, 0.0], 0.0)], 0.1);
        assert_eq!(c.surfaces(&deck(), &[pose(7, [5.0, 5.0, 0.0], 0.0)])[0].velocity, [0.0; 3]);
    }

    fn sailed(from: ShipPose, to: ShipPose) -> BoatCarry {
        let mut c = BoatCarry::default();
        c.advance(&[from], 0.1);
        c.advance(&[to], 0.1);
        c
    }

    #[test]
    fn a_rider_moves_with_the_ship_they_stand_on() {
        let c = sailed(pose(7, [0.0, 0.0, 0.0], 0.0), pose(7, [3.0, -2.0, 0.0], 0.0));
        let p = c.carry(Some(SurfaceId(BOAT_ID_BASE + 7)), [10.0, 5.0, 8.0]);
        assert!((p[0] - 13.0).abs() < 1e-4 && (p[1] - 3.0).abs() < 1e-4 && p[2] == 8.0, "{p:?}");
    }

    #[test]
    fn a_rider_swings_around_the_ship_when_it_turns() {
        // 90 degrees of heading is a quarter turn counterclockwise: a rider east of the ship ends
        // north of it.
        let c = sailed(pose(7, [100.0, 100.0, 0.0], 0.0), pose(7, [100.0, 100.0, 0.0], 90.0));
        let p = c.carry(Some(SurfaceId(BOAT_ID_BASE + 7)), [110.0, 100.0, 3.0]);
        assert!((p[0] - 100.0).abs() < 1e-3 && (p[1] - 110.0).abs() < 1e-3, "{p:?}");
    }

    #[test]
    fn a_rider_off_the_ship_stays_put() {
        let c = sailed(pose(7, [0.0, 0.0, 0.0], 0.0), pose(7, [3.0, 0.0, 0.0], 0.0));
        assert_eq!(c.carry(None, [1.0, 2.0, 3.0]), [1.0, 2.0, 3.0]);
        assert_eq!(c.carry(Some(SurfaceId(5)), [1.0, 2.0, 3.0]), [1.0, 2.0, 3.0], "a lift, not a ship");
    }

    #[test]
    fn a_ship_the_server_moved_does_not_fling_its_rider() {
        let c = sailed(pose(7, [0.0, 0.0, 0.0], 0.0), pose(7, [400.0, 0.0, 0.0], 0.0));
        assert_eq!(c.carry(Some(SurfaceId(BOAT_ID_BASE + 7)), [1.0, 0.0, 3.0]), [1.0, 0.0, 3.0]);
    }

    #[test]
    fn a_ship_seen_for_the_first_time_carries_nobody() {
        let mut c = BoatCarry::default();
        c.advance(&[pose(7, [50.0, 0.0, 0.0], 0.0)], 0.1);
        assert_eq!(c.carry(Some(SurfaceId(BOAT_ID_BASE + 7)), [1.0, 0.0, 3.0]), [1.0, 0.0, 3.0]);
    }

    #[test]
    fn a_ship_that_left_carries_nobody() {
        let mut c = BoatCarry::default();
        c.advance(&[pose(7, [0.0, 0.0, 0.0], 0.0)], 0.1);
        c.advance(&[], 0.1);
        assert_eq!(c.carry(Some(SurfaceId(BOAT_ID_BASE + 7)), [1.0, 0.0, 3.0]), [1.0, 0.0, 3.0]);
    }

    /// The carry must turn a rider the way the renderer turns the drawn deck: a deck point placed at
    /// the old pose and carried to the new one lands where the deck point is drawn at the new pose.
    /// This ties the carry direction to `entity_model_matrix_static` and not to its own tests.
    #[test]
    fn the_carry_follows_the_deck_the_renderer_draws() {
        let bounds = ModelBounds { y_bottom: 24.0, y_extent: 100.0, x_center: 3.0, z_center: -2.0 };
        let deck = ShipDeck::from_meshes(&[tri_at(25.0, 1.0)], bounds);
        let deck_point = |at: &ShipPose| deck.surface(at, [0.0; 3]).tris[0][1];
        for (h0, h1) in [(0.0, 30.0), (100.0, 70.0), (250.0, 251.0), (359.0, 1.0), (2.0, 358.0)] {
            let (p0, p1) = (pose(7, [100.0, -40.0, 5.0], h0), pose(7, [103.0, -38.0, 5.0], h1));
            let mut c = BoatCarry::default();
            c.advance(&[p0], 0.1);
            c.advance(&[p1], 0.1);
            let carried = c.carry(Some(p0.surface_id()), deck_point(&p0));
            let drawn = deck_point(&p1);
            assert!((carried[0] - drawn[0]).abs() < 1e-3 && (carried[1] - drawn[1]).abs() < 1e-3,
                "heading {h0} to {h1}: carried {carried:?}, drawn {drawn:?}");
        }
    }

    /// The same deck point at the two poses differs, so the test above cannot pass by a no-op carry.
    #[test]
    fn a_turn_moves_the_drawn_deck_point() {
        let deck = deck();
        let a = deck.surface(&pose(7, [0.0; 3], 0.0), [0.0; 3]).tris[0][1];
        let b = deck.surface(&pose(7, [0.0; 3], 30.0), [0.0; 3]).tris[0][1];
        assert!((a[0] - b[0]).hypot(a[1] - b[1]) > 1.0, "{a:?} {b:?}");
    }

    /// The real galleon: its main deck must stand level with a dock 12 units above the water, and
    /// the crow's nests must be left out.
    #[test]
    #[ignore = "needs ship.glb: set $EQOXIDE_ZONE_ASSETS to a local baked-asset models dir"]
    fn the_real_ship_deck_is_flush_with_the_dock() {
        let dir = std::env::var("EQOXIDE_ZONE_ASSETS").expect("EQOXIDE_ZONE_ASSETS");
        let deck = ShipDeck::load(&Path::new(&dir).join("ship.glb")).unwrap();
        assert!(deck.len() > 20, "{} deck triangles", deck.len());
        let s = deck.surface(&pose(7, [0.0, 0.0, 0.0], 0.0), [0.0; 3]);
        let (lo, hi) = s.tris.iter().flatten()
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v[2]), hi.max(v[2])));
        println!("deck {} tris, world z {lo:.2}..{hi:.2}, center {:?}", s.tris.len(), s.center());
        assert!((11.0..=14.0).contains(&lo), "lowest deck {lo}");
        assert!(hi < 30.0, "highest deck {hi}");
    }
}
