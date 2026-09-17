//! Client-side visibility filtering (spec §6): distance cutoff plus line-of-sight occlusion (Task
//! 9), computed each tick from the character's actual position — not the full zone's worth of
//! entity updates. No renderer/GPU dependency; `VISIBILITY_DIST` mirrors the renderer's
//! `ENTITY_DRAW_DIST` (`crates/eqoxide-renderer/src/pass.rs:367`, value 500.0) rather than
//! importing it, since this crate must not depend on eqoxide-renderer.

use eqoxide_agent_protocol::observation::VisibleEntity;
use eqoxide_core::game_state::Entity;
use eqoxide_nav::collision::SharedCollision;
use std::collections::HashMap;

pub const VISIBILITY_DIST: f32 = 500.0;

fn to_visible_entity(e: &Entity) -> VisibleEntity {
    VisibleEntity {
        spawn_id: e.spawn_id,
        name: e.name.clone(),
        is_npc: e.is_npc,
        level: e.level,
        race: e.race.clone(),
        pos: [e.x, e.y, e.z],
        heading: e.heading,
        hp_pct: e.hp_pct,
        dead: e.dead,
    }
}

fn within_distance(e: &Entity, from: [f32; 3], max_dist: f32) -> bool {
    let d = [e.x - from[0], e.y - from[1], e.z - from[2]];
    let dist2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    dist2 <= max_dist * max_dist
}

/// A CHEST-HEIGHT ray between the two positions, using the character's own body dimensions — a
/// foot-height ray false-trips on ground undulation; chest height rides above it while a real wall
/// still blocks it (same reasoning `eqoxide_nav::collision::carrot_los_clear` documents for its own,
/// unrelated use case — see this task's design note in the plan). `false` (occluded) when no zone
/// geometry is loaded at all, matching agent-honesty: no visibility claim without real geometry.
fn has_line_of_sight(collision: &SharedCollision, from: [f32; 3], to: [f32; 3]) -> bool {
    let chest = eqoxide_nav::traversability::PLAYER_BODY.chest;
    let radius = eqoxide_core::physics::PLAYER_RADIUS;
    let guard = collision.read().unwrap();
    let Some(col) = guard.as_ref() else { return false };
    let eye = [from[0], from[1], from[2] + chest];
    let target = [to[0], to[1], to[2] + chest];
    col.line_clear(eye, target, radius)
}

/// Entities within `max_dist` of `from` with clear line of sight (spec §6). No facing/FOV cone.
pub fn visible_entities(
    entities: &HashMap<u32, Entity>,
    from: [f32; 3],
    max_dist: f32,
    collision: &SharedCollision,
) -> Vec<VisibleEntity> {
    entities
        .values()
        .filter(|e| within_distance(e, from, max_dist))
        .filter(|e| has_line_of_sight(collision, from, [e.x, e.y, e.z]))
        .map(to_visible_entity)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
    use eqoxide_nav::collision::Collision;
    use std::sync::{Arc, RwLock};

    fn entity_at(spawn_id: u32, pos: [f32; 3]) -> Entity {
        Entity {
            spawn_id,
            name: format!("entity{spawn_id}"),
            level: 1,
            is_npc: true,
            x: pos[0],
            y: pos[1],
            z: pos[2],
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: "Rat".into(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9],
            equipment_tint: [[0; 3]; 9],
            gender: 0,
            helm: 0,
            showhelm: 0,
            face: 0,
            hairstyle: 0,
            haircolor: 0,
            pose: eqoxide_core::game_state::Pose::default(),
            gait: None,
            is_boat: false,
            flymode: 0,
            npc_tint_index: 0,
        }
    }

    fn floor() -> MeshData {
        MeshData {
            positions: vec![[0.0, 0.0, 0.0], [40.0, 0.0, 0.0], [40.0, 0.0, 40.0], [0.0, 0.0, 40.0]],
            normals: vec![[0.0, 1.0, 0.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0; 3],
            render_mode: RenderMode::Opaque,
            anim: None,
        }
    }

    // A full-width wall panel at north = `n` (GLB axes -> world: east = p[2], north = p[0], height
    // = p[1] — same convention as eqoxide-nav's own `slotted_wall` test fixture).
    fn wall_at(n: f32) -> MeshData {
        MeshData {
            positions: vec![[0.0, 0.0, n], [40.0, 0.0, n], [40.0, 10.0, n], [0.0, 10.0, n]],
            normals: vec![[0.0, 0.0, 1.0]; 4],
            uvs: vec![[0.0, 0.0]; 4],
            indices: vec![0, 1, 2, 0, 2, 3],
            texture_name: None,
            base_color: [1.0; 4],
            center: [0.0; 3],
            render_mode: RenderMode::Opaque,
            anim: None,
        }
    }

    fn shared(col: Collision) -> SharedCollision {
        Arc::new(RwLock::new(Some(Arc::new(col))))
    }

    fn open_collision() -> SharedCollision {
        shared(Collision::build(&ZoneAssets { terrain: vec![floor()], objects: vec![], textures: vec![] }, 2.0))
    }

    fn walled_collision() -> SharedCollision {
        shared(Collision::build(
            &ZoneAssets { terrain: vec![floor(), wall_at(20.0)], objects: vec![], textures: vec![] },
            2.0,
        ))
    }

    #[test]
    fn entity_within_distance_and_clear_los_is_included() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [10.0, 5.0, 0.0]));
        let result = visible_entities(&entities, [10.0, 0.0, 0.0], VISIBILITY_DIST, &open_collision());
        assert_eq!(result.len(), 1, "an entity 5 units away with no obstruction must be visible");
        assert_eq!(result[0].spawn_id, 1);
    }

    #[test]
    fn entity_beyond_distance_is_excluded() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [1000.0, 5.0, 0.0]));
        let result = visible_entities(&entities, [10.0, 0.0, 0.0], VISIBILITY_DIST, &open_collision());
        assert!(result.is_empty(), "an entity 1000 units away must be excluded by the 500-unit cutoff");
    }

    #[test]
    fn entity_behind_a_wall_is_excluded_despite_being_in_range() {
        let mut entities = HashMap::new();
        // Player at x=10, entity at x=30 — the wall at x=20 sits directly between them.
        entities.insert(1, entity_at(1, [30.0, 5.0, 5.0]));
        let result = visible_entities(&entities, [10.0, 5.0, 5.0], VISIBILITY_DIST, &walled_collision());
        assert!(result.is_empty(), "a wall between the two points must occlude the entity");
    }

    #[test]
    fn entity_in_front_of_the_wall_with_clear_los_is_included() {
        let mut entities = HashMap::new();
        // Both player and entity are on the SAME side (x < 20) of the wall at x=20.
        entities.insert(1, entity_at(1, [15.0, 5.0, 5.0]));
        let result = visible_entities(&entities, [10.0, 5.0, 5.0], VISIBILITY_DIST, &walled_collision());
        assert_eq!(result.len(), 1, "nothing obstructs a line that never crosses the wall");
    }
}
