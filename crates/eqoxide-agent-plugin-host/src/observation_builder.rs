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
        target_id: gs.target_id,
        target_name: gs.target_name.clone(),
        auto_attack: gs.auto_attack,
        sitting: gs.sitting,
        held: gs.player_hold.is_some(),
        player_class: gs.player_class.clone(),
        player_level: gs.player_level,
    }
}

/// `tick`: the tick loop's own monotonic per-connection counter (spec §9's only way for the agent
/// to detect dropped/bursted ticks, since `Observation` is the sole outbound message shape) —
/// owned by the caller (`session::run`'s tick loop), not this pure builder function.
pub fn build_observation(
    gs: &GameState,
    collision: &SharedCollision,
    spells: &SpellDb,
    net_thread_dead: &NetThreadDeadShared,
    tick: u64,
) -> Observation {
    // No zone collision geometry loaded (startup, zone transitions, `--testzone`'s synthetic debug
    // zone) is NOT the same claim as "nothing is visible" — computed before the vision filter call
    // below so `visibility_available` reflects the same `collision` read the filter itself uses.
    let visibility_available = collision.read().unwrap().is_some();
    // `gs.world.entities` is a HashMap, so `visible_entities`'s iteration order is arbitrary and
    // can differ between two ticks with the exact same entity set — sort by spawn_id so the wire
    // output is deterministic and diffable across ticks.
    let mut visible = eqoxide_agent_vision_filter::visible_entities(
        &gs.world.entities,
        [gs.player_x, gs.player_y, gs.player_z],
        eqoxide_agent_vision_filter::VISIBILITY_DIST,
        collision,
    );
    visible.sort_unstable_by_key(|e| e.spawn_id);
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
        visibility_available,
        tick,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_assets::{MeshData, RenderMode, ZoneAssets};
    use eqoxide_core::game_state::{make_entity, GameState};
    use eqoxide_core::spells::SpellDb;
    use eqoxide_nav::collision::Collision;
    use std::sync::{Arc, Mutex, RwLock};

    fn empty_collision() -> SharedCollision {
        Arc::new(RwLock::new(None))
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

    /// Real (if trivial) collision geometry, mirroring `eqoxide-agent-vision-filter`'s own
    /// `open_collision()` test fixture — for Fix 6's `visibility_available: true` branch, which
    /// `empty_collision()` above can never exercise.
    fn populated_collision() -> SharedCollision {
        Arc::new(RwLock::new(Some(Arc::new(Collision::build(
            &ZoneAssets { terrain: vec![floor()], objects: vec![], textures: vec![] },
            2.0,
        )))))
    }

    fn no_death() -> NetThreadDeadShared {
        Arc::new(Mutex::new(None))
    }

    #[test]
    fn own_state_reflects_position_health_and_zone() {
        let mut gs = GameState {
            player_x: 1.0,
            player_y: 2.0,
            player_z: 3.0,
            player_heading: 45.0,
            cur_hp: 80,
            max_hp: 100,
            ..Default::default()
        };
        gs.world.zone_name = "qeynos".into();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
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
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert_eq!(obs.own.endurance, 80);
        assert_eq!(obs.own.endurance_max, 100);
        assert!(obs.own.endurance_confirmed, "set_endurance is the authoritative OP_EnduranceUpdate path");
    }

    #[test]
    fn endurance_confirmed_is_false_before_any_endurance_update_is_seen() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
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
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
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
        let gs = GameState { player_dead: true, ..Default::default() };
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert!(obs.dead);
    }

    #[test]
    fn terminated_is_false_when_net_thread_dead_is_none() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert!(!obs.terminated);
    }

    #[test]
    fn terminated_is_true_when_net_thread_dead_is_some() {
        use eqoxide_ipc::{NetThreadDeath, NetThreadEnd};
        let gs = GameState::default();
        let dead: NetThreadDeadShared =
            Arc::new(Mutex::new(Some(NetThreadDeath::new(NetThreadEnd::Panicked, "test"))));
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &dead, 0);
        assert!(obs.terminated);
    }

    #[test]
    fn truncated_is_always_false() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert!(!obs.truncated);
    }

    #[test]
    fn visible_entities_come_from_world_entities_via_the_vision_filter() {
        // player_x/y/z default to 0.0 already — no reassignment needed.
        let mut gs = GameState::default();
        gs.world.entities.insert(7, make_entity(7, "a_rat00", 5.0, 0.0, 0.0, true));
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        // No zone geometry loaded (empty_collision) -> has_line_of_sight returns false for everyone
        // (agent-honesty: no visibility claim without real geometry) -> visible is empty. This
        // confirms the wiring reaches the vision filter at all, which is what this test checks.
        assert!(obs.visible.is_empty(), "no collision grid loaded means no LOS can be confirmed");
        assert!(
            !obs.visibility_available,
            "agent-honesty: no zone geometry loaded means no confident visibility claim either way"
        );
    }

    #[test]
    fn visibility_available_is_true_once_real_collision_geometry_is_loaded() {
        let gs = GameState { player_x: 10.0, player_y: 0.0, player_z: 0.0, ..Default::default() };
        let obs = build_observation(&gs, &populated_collision(), &SpellDb::default(), &no_death(), 0);
        assert!(
            obs.visibility_available,
            "real collision geometry is loaded, so a confident visibility claim is now possible"
        );
    }

    #[test]
    fn visible_entity_with_clear_los_appears_once_geometry_is_loaded() {
        let mut gs = GameState { player_x: 10.0, player_y: 0.0, player_z: 0.0, ..Default::default() };
        // Mirrors eqoxide-agent-vision-filter's own `entity_within_distance_and_clear_los_is_included`
        // fixture exactly (self at [10,0,0], target 5 units up at [10,5,0], both over the floor mesh).
        gs.world.entities.insert(1, make_entity(1, "a_rat00", 10.0, 5.0, 0.0, true));
        let obs = build_observation(&gs, &populated_collision(), &SpellDb::default(), &no_death(), 0);
        assert_eq!(obs.visible.len(), 1, "an unobstructed entity in range must be reported visible");
        assert_eq!(obs.visible[0].spawn_id, 1);
    }

    #[test]
    fn own_state_carries_target_combat_and_class_fields() {
        let gs = GameState {
            target_id: Some(42),
            target_name: Some("a_rat00".into()),
            auto_attack: true,
            sitting: true,
            player_class: "Warrior".into(),
            player_level: 10,
            ..Default::default()
        };
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert_eq!(obs.own.target_id, Some(42));
        assert_eq!(obs.own.target_name.as_deref(), Some("a_rat00"));
        assert!(obs.own.auto_attack);
        assert!(obs.own.sitting);
        assert_eq!(obs.own.player_class, "Warrior");
        assert_eq!(obs.own.player_level, 10);
    }

    #[test]
    fn held_reflects_player_hold_as_a_plain_bool_not_the_raw_reason() {
        use eqoxide_core::game_state::{ControllerHold, ControllerHoldReason};

        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert!(!obs.own.held, "no hold in effect by default");

        let held_gs = GameState {
            player_hold: Some(ControllerHold { reason: ControllerHoldReason::EmbeddedNoRecovery, secs: 1.0 }),
            ..Default::default()
        };
        let held_obs = build_observation(&held_gs, &empty_collision(), &SpellDb::default(), &no_death(), 0);
        assert!(held_obs.own.held, "a Some(ControllerHold) must surface as held: true on the wire");
    }

    #[test]
    fn visible_entities_are_sorted_by_spawn_id_regardless_of_hashmap_iteration_order() {
        let mut gs = GameState { player_x: 10.0, player_y: 0.0, player_z: 0.0, ..Default::default() };
        // Inserted deliberately out of order — entities live in a HashMap, so nothing about
        // insertion order should determine the wire output's order.
        gs.world.entities.insert(9, make_entity(9, "a_rat09", 10.0, 5.0, 0.0, true));
        gs.world.entities.insert(2, make_entity(2, "a_rat02", 10.0, 6.0, 0.0, true));
        gs.world.entities.insert(5, make_entity(5, "a_rat05", 10.0, 7.0, 0.0, true));
        let obs = build_observation(&gs, &populated_collision(), &SpellDb::default(), &no_death(), 0);
        let ids: Vec<u32> = obs.visible.iter().map(|e| e.spawn_id).collect();
        assert_eq!(ids, vec![2, 5, 9], "visible must be sorted ascending by spawn_id");
    }

    #[test]
    fn tick_is_threaded_straight_through_from_the_caller() {
        let gs = GameState::default();
        let obs = build_observation(&gs, &empty_collision(), &SpellDb::default(), &no_death(), 7);
        assert_eq!(obs.tick, 7, "build_observation must not silently reinterpret the caller's counter");
    }
}
