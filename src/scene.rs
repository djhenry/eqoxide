use crate::game_state::GameState;

/// Billboard for one entity in the scene.
#[derive(Debug, Clone)]
pub struct Billboard {
    pub id:        u32,
    pub pos:       [f32; 3],
    pub level:     u32,
    pub hp_pct:    f32,
    pub is_target: bool,
    pub dead:      bool,
    pub name:      String,
    pub race:      String,
    pub action:    String,
    pub heading:   f32,
}

/// A single entry in the message log.
#[derive(Debug, Clone)]
pub struct LogEntry {
    pub kind: String,
    pub text: String,
    pub timestamp: std::time::Instant,
}

/// All data the renderer needs for one frame.
#[derive(Debug, Default, Clone)]
pub struct SceneState {
    pub zone: String,
    pub zone_changed: bool,
    pub player_pos: [f32; 3],
    pub player_heading: f32,
    pub player_hp_pct: f32,
    pub player_mana_pct: f32,
    pub player_xp_pct: f32,
    pub player_name: String,
    pub player_level: u32,
    pub player_race: String,
    pub player_action: String,
    pub target_name: Option<String>,
    pub target_hp_pct: Option<f32>,
    pub target_con: Option<[u8; 3]>,
    pub strategy: String,
    pub billboards: Vec<Billboard>,
    pub messages: Vec<LogEntry>,
}

impl SceneState {
    /// Build SceneState from a live GameState snapshot.
    pub fn from_game_state(gs: &GameState) -> Self {
        let billboards = gs.entities.values().map(|e| Billboard {
            id:        e.spawn_id,
            pos:       [e.y, e.x, e.z], // GPU [east=server_y, north=server_x, height]
            level:     e.level,
            hp_pct:    e.hp_pct,
            is_target: gs.target_id == Some(e.spawn_id),
            dead:      e.dead,
            name:      e.name.clone(),
            race:      e.race.clone(),
            action:    String::new(),
            heading:   e.heading,
        }).collect();

        let messages = gs.messages.iter().map(|m| LogEntry {
            kind: m.kind.clone(),
            text: m.text.clone(),
            timestamp: m.timestamp,
        }).collect();

        SceneState {
            zone: gs.zone_name.clone(),
            zone_changed: gs.zone_changed,
            // Zone geometry is uploaded as GPU [east=server_y, north=server_x, height=server_z].
            // Swap X/Y here so scene coordinates match GPU space throughout the render pipeline.
            player_pos: [gs.player_y, gs.player_x, gs.player_z],
            player_heading: gs.player_heading,
            player_hp_pct: gs.hp_pct,
            player_mana_pct: gs.mana_pct,
            player_xp_pct: gs.xp_pct,
            player_name: gs.player_name.clone(),
            player_level: gs.player_level,
            player_race: gs.player_race.clone(),
            player_action: gs.player_action.clone(),
            target_name: gs.target_name.clone(),
            target_hp_pct: gs.target_hp_pct,
            target_con: gs.target_con,
            strategy: gs.strategy.clone(),
            billboards,
            messages,
        }
    }
}

impl Default for LogEntry {
    fn default() -> Self {
        LogEntry {
            kind: String::new(),
            text: String::new(),
            timestamp: std::time::Instant::now(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_state::{GameState, Entity};

    fn sample_state() -> GameState {
        let mut gs = GameState::new();
        gs.zone_name = "qeynoshills".into();
        gs.zone_changed = false;
        gs.player_name = "Aethas".into();
        gs.player_level = 5;
        gs.hp_pct = 87.3;
        gs.mana_pct = 75.0;
        gs.xp_pct = 12.5;
        gs.player_x = 1.0;
        gs.player_y = 2.0;
        gs.player_z = 3.0;
        gs.player_heading = 192.0;
        gs.player_race = "HUM".into();
        gs.player_action = "walking".into();
        gs.target_id = Some(42);
        gs.target_name = Some("a gnoll".into());
        gs.target_hp_pct = Some(61.0);
        gs.strategy = "Attacking".into();

        gs.upsert_entity(Entity {
            spawn_id: 42,
            name: "a gnoll".into(),
            level: 4,
            is_npc: true,
            x: 10.0, y: 20.0, z: 3.0,
            hp_pct: 61.0,
            cur_hp: 61,
            max_hp: 100,
            race: "GNL".into(),
            heading: 0.0,
            dead: false,
        });

        gs
    }

    #[test]
    fn from_game_state_sets_player_fields() {
        let scene = SceneState::from_game_state(&sample_state());
        assert_eq!(scene.player_name, "Aethas");
        assert_eq!(scene.player_pos, [2.0, 1.0, 3.0]); // [server_y, server_x, server_z] GPU order
        assert_eq!(scene.player_heading, 192.0);
    }

    #[test]
    fn from_game_state_marks_target_billboard() {
        let scene = SceneState::from_game_state(&sample_state());
        assert_eq!(scene.billboards.len(), 1);
        assert!(scene.billboards[0].is_target);
    }

    #[test]
    fn from_game_state_no_target_no_is_target() {
        let mut gs = sample_state();
        gs.target_id = None;
        let scene = SceneState::from_game_state(&gs);
        assert!(!scene.billboards[0].is_target);
    }

    #[test]
    fn from_game_state_billboard_race_propagated() {
        let gs = sample_state();
        let scene = SceneState::from_game_state(&gs);
        assert_eq!(scene.billboards[0].race, "GNL");
    }

    #[test]
    fn from_game_state_billboard_id_propagated() {
        let scene = SceneState::from_game_state(&sample_state());
        assert_eq!(scene.billboards[0].id, 42);
    }

    #[test]
    fn from_game_state_zone_name() {
        let scene = SceneState::from_game_state(&sample_state());
        assert_eq!(scene.zone, "qeynoshills");
    }
}
