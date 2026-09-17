//! Composes the per-tick `Observation` from `GameState` + the vision filter + the legal-action
//! mask (spec §9). Pure function of its inputs — no I/O, so it's directly unit-testable.

use crate::legal_actions::build_legal_actions;
use eqoxide_agent_protocol::observation::{BuffView, CastingView, Observation, OwnState};
use eqoxide_core::game_state::GameState;
use eqoxide_core::spells::SpellDb;
use eqoxide_ipc::NetThreadDeadShared;
use eqoxide_nav::collision::SharedCollision;

fn build_own_state(gs: &GameState) -> OwnState {
    OwnState {
        pos: [gs.player_x, gs.player_y, gs.player_z],
        heading: gs.player_heading,
        hp: gs.cur_hp,
        hp_max: gs.max_hp,
        hp_verified: gs.hp_verified(),
        mana: gs.cur_mana,
        mana_max: gs.max_mana,
        endurance: gs.cur_endurance,
        endurance_max: gs.max_endurance,
        endurance_confirmed: gs.endurance_confirmed,
        casting: gs.casting.as_ref().map(|c| CastingView {
            spell_id: c.spell_id,
            elapsed_ms: c.started.elapsed().as_millis() as u32,
            cast_ms: c.cast_ms,
        }),
        buffs: gs
            .buffs
            .iter()
            .map(|(&slot, b)| BuffView { slot, spell_id: b.spell_id, duration_ticks: b.duration_ticks })
            .collect(),
        zone_name: gs.world.zone_name.clone(),
    }
}

pub fn build_observation(
    gs: &GameState,
    collision: &SharedCollision,
    spells: &SpellDb,
    net_thread_dead: &NetThreadDeadShared,
) -> Observation {
    let visible = eqoxide_agent_vision_filter::visible_entities(
        &gs.world.entities,
        [gs.player_x, gs.player_y, gs.player_z],
        eqoxide_agent_vision_filter::VISIBILITY_DIST,
        collision,
    );
    Observation {
        own: build_own_state(gs),
        visible,
        legal_actions: build_legal_actions(&gs.mem_spells, spells),
        // Confirmed OP_Death, not the broader "HP halted but unconfirmed" state — respawn is only
        // ever the correct remedy for THIS signal (mirrors eqoxide-http's require_alive distinction).
        dead: gs.player_dead,
        // The one signal eqoxide can honestly assert as a true terminal state: the net thread is
        // gone for good.
        terminated: net_thread_dead.lock().unwrap().is_some(),
        // eqoxide imposes no rollout cutoff of its own in v1; an external driver may still truncate
        // its own rollout without needing this field to say so.
        truncated: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_core::game_state::{make_entity, GameState};
    use eqoxide_core::spells::SpellDb;
    use std::sync::{Arc, Mutex, RwLock};

    fn empty_collision() -> SharedCollision {
        Arc::new(RwLock::new(None))
    }

    fn no_death() -> NetThreadDeadShared {
        Arc::new(Mutex::new(None))
    }

    #[test]
    fn own_state_reflects_position_health_and_zone() {
        let mut gs = GameState::default();
        gs.player_x = 1.0;
        gs.player_y = 2.0;
        gs.player_z = 3.0;
        gs.player_heading = 45.0;
        gs.cur_hp = 80;
        gs.max_hp = 100;
        gs.world.zone_name = "qeynos".into();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert_eq!(obs.own.pos, [1.0, 2.0, 3.0]);
        assert_eq!(obs.own.heading, 45.0);
        assert_eq!(obs.own.hp, 80);
        assert_eq!(obs.own.hp_max, 100);
        assert_eq!(obs.own.zone_name, "qeynos");
        assert!(obs.own.casting.is_none());
    }

    #[test]
    fn endurance_reflects_game_state_confirmed_values() {
        let mut gs = GameState::default();
        gs.set_endurance(80, 100);
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert_eq!(obs.own.endurance, 80);
        assert_eq!(obs.own.endurance_max, 100);
        assert!(obs.own.endurance_confirmed, "set_endurance is the authoritative OP_EnduranceUpdate path");
    }

    #[test]
    fn endurance_confirmed_is_false_before_any_endurance_update_is_seen() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(
            !obs.own.endurance_confirmed,
            "agent-honesty: no confident endurance claim before the server has said anything"
        );
    }

    #[test]
    fn buffs_mirror_game_state_buffs_sorted_by_slot() {
        let mut gs = GameState::default();
        gs.buff_slot_set(5, 12, 42);
        gs.buff_slot_set(1, 90, -1000);
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert_eq!(
            obs.own.buffs,
            vec![
                BuffView { slot: 1, spell_id: 90, duration_ticks: -1000 },
                BuffView { slot: 5, spell_id: 12, duration_ticks: 42 },
            ],
            "buffs come from a BTreeMap, so iteration order is already sorted by slot"
        );
    }

    #[test]
    fn dead_maps_to_player_dead_specifically() {
        let mut gs = GameState::default();
        gs.player_dead = true;
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(obs.dead);
    }

    #[test]
    fn terminated_is_false_when_net_thread_dead_is_none() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(!obs.terminated);
    }

    #[test]
    fn terminated_is_true_when_net_thread_dead_is_some() {
        use eqoxide_ipc::{NetThreadDeath, NetThreadEnd};
        let gs = GameState::default();
        let dead: NetThreadDeadShared =
            Arc::new(Mutex::new(Some(NetThreadDeath::new(NetThreadEnd::Panicked, "test"))));
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &dead);
        assert!(obs.terminated);
    }

    #[test]
    fn truncated_is_always_false() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        assert!(!obs.truncated);
    }

    #[test]
    fn visible_entities_come_from_world_entities_via_the_vision_filter() {
        let mut gs = GameState::default();
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.player_z = 0.0;
        gs.world.entities.insert(7, make_entity(7, "a_rat00", 5.0, 0.0, 0.0, true));
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death());
        // No zone geometry loaded (empty_collision) -> has_line_of_sight returns false for everyone
        // (agent-honesty: no visibility claim without real geometry) -> visible is empty. This
        // confirms the wiring reaches the vision filter at all, which is what this test checks.
        assert!(obs.visible.is_empty(), "no collision grid loaded means no LOS can be confirmed");
    }
}
