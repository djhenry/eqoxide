//! Client-side visibility filtering (spec §6): distance cutoff plus line-of-sight occlusion (Task
//! 9), computed each tick from the character's actual position — not the full zone's worth of
//! entity updates. No renderer/GPU dependency; `VISIBILITY_DIST` mirrors the renderer's
//! `ENTITY_DRAW_DIST` (`crates/eqoxide-renderer/src/pass.rs:367`, value 500.0) rather than
//! importing it, since this crate must not depend on eqoxide-renderer.

use eqoxide_agent_protocol::observation::VisibleEntity;
use eqoxide_core::game_state::Entity;
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

/// Entities within `max_dist` of `from` — no line-of-sight check yet (Task 9 adds occlusion).
pub fn visible_entities(entities: &HashMap<u32, Entity>, from: [f32; 3], max_dist: f32) -> Vec<VisibleEntity> {
    entities.values().filter(|e| within_distance(e, from, max_dist)).map(to_visible_entity).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn entity_within_distance_is_included() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [10.0, 0.0, 0.0]));
        let result = visible_entities(&entities, [0.0, 0.0, 0.0], VISIBILITY_DIST);
        assert_eq!(result.len(), 1, "an entity 10 units away is well within the 500-unit cutoff");
        assert_eq!(result[0].spawn_id, 1);
    }

    #[test]
    fn entity_beyond_distance_is_excluded() {
        let mut entities = HashMap::new();
        entities.insert(1, entity_at(1, [1000.0, 0.0, 0.0]));
        let result = visible_entities(&entities, [0.0, 0.0, 0.0], VISIBILITY_DIST);
        assert!(result.is_empty(), "an entity 1000 units away must be excluded by the 500-unit cutoff");
    }
}
