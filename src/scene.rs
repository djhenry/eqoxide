//! `SceneState` — the render thread's per-frame snapshot of what to draw (entity billboards, player
//! pose/position, recent messages, target info, …). Copied from the network-owned `GameState` once
//! per frame so the render loop never blocks on or shares locks with the EQ network thread.

use crate::game_state::GameState;

/// How long a one-shot combat swing (OP_Animation) plays before reverting to idle/walk. ~one swing.
pub const COMBAT_SWING_WINDOW: std::time::Duration = std::time::Duration::from_millis(600);

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
    pub equipment: [u32; 9],
    pub equipment_tint: [[u8; 3]; 9],
    pub gender:    u8,
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
    pub player_class: String,
    pub player_gender: u8,
    pub coin: [u32; 4],
    pub stats: [u32; 7],
    pub player_action: String,
    pub target_name: Option<String>,
    pub target_hp_pct: Option<f32>,
    pub target_con: Option<[u8; 3]>,
    pub strategy: String,
    pub billboards: Vec<Billboard>,
    pub messages: Vec<LogEntry>,
    /// Item material IDs for each equipment slot (0..9), from the player profile.
    pub player_equipment: [u32; 9],
    /// RGB tint for each equipment slot (0..9), from the player profile.
    pub player_equipment_tint: [[u8; 3]; 9],
    /// Player inventory + equipment items (for the inventory UI window).
    pub inventory: Vec<crate::game_state::InvItem>,
    /// Equipped weapon held-model ids (IDFile, e.g. "IT10649"), for rendering weapons in hand.
    /// Empty = nothing equipped in that slot. Primary = worn slot 13, secondary = slot 14.
    pub primary_weapon_idfile: String,
    pub secondary_weapon_idfile: String,
    /// Memorized spell gem IDs (9 slots); 0xFFFF_FFFF = empty slot.
    pub mem_spells: [u32; 9],
    /// Active cast in progress (Some) or idle (None).
    pub casting: Option<crate::game_state::CastState>,
    /// True when the player is sitting.
    pub sitting: bool,
    /// True when auto-attack is enabled.
    pub auto_attack: bool,
    /// The spawn_id of the current target, if any.
    pub target_id: Option<u32>,
}

impl SceneState {
    /// Populate billboards with one entry per loaded archetype for the test zone.
    /// Each model is placed side-by-side along the east axis so every archetype
    /// can be visually inspected.
    pub fn inject_test_billboards(&mut self) {
        use crate::models::race_to_archetype;
        use std::collections::HashSet;

        // EQ race codes that map to distinct archetypes. Each entry is
        // (race_code, archetype_key, name_label).
        // Archetypes without converted GLB models are skipped at render time.
        let archetypes: Vec<(&str, &str, &str)> = vec![
            ("HUM", "humanoid",  "Humanoid"),
            ("ELF", "elf",       "Elf"),
            ("DWF", "dwarf",     "Dwarf"),
            ("GNL", "gnoll",     "Gnoll"),
            ("FRG", "frog",      "Frog"),
            ("SKE", "skeleton",  "Skeleton"),
            ("ZOM", "zombie",    "Zombie"),
            ("BEA", "bear",      "Bear"),
            ("WOL", "wolf",      "Wolf"),
            ("RAT", "rat",       "Rat"),
            ("SNA", "snake",     "Snake"),
            ("BAT", "bat",       "Bat"),
            ("BRD", "bird",      "Bird"),
            ("WSP", "wasp",      "Wasp"),
            ("WRM", "worm",      "Worm"),
        ];

        // Deduplicate archetypes (e.g. WOL/LIO/CAT all map to "wolf").
        let mut seen = HashSet::new();
        let mut unique: Vec<(&str, &str, &str)> = Vec::new();
        for entry in &archetypes {
            let arch = race_to_archetype(entry.0);
            if seen.insert(arch) {
                unique.push(*entry);
            }
        }

        let spacing = 20.0_f32; // east spacing between models
        let start_east = -((unique.len() as f32) * spacing * 0.5); // center around origin

        for (i, &(race, _arch, label)) in unique.iter().enumerate() {
            let east = start_east + i as f32 * spacing;
            self.billboards.push(crate::scene::Billboard {
                id:        1000 + i as u32,
                pos:       [east, 0.0, 0.0], // [east, north, height]
                level:     50,
                hp_pct:    100.0,
                is_target: false,
                dead:      false,
                name:      format!("Test_{}", label),
                race:      race.to_string(),
                action:    "idle".to_string(),
                heading:   0.0,
                equipment:      [0; 9],
                equipment_tint: [[0; 3]; 9],
                gender:    0,
            });
        }

        eprintln!("testzone: injected {} billboards for character model inspection",
                  self.billboards.len());
    }

    /// Build SceneState from a live GameState snapshot.
    pub fn from_game_state(gs: &GameState) -> Self {
        let billboards = gs.entities.values().map(|e| {
            // Map EQ Animation:: values to action strings for clip resolution.
            // Animation constants from eq_constants.h: Standing=100, Freeze=102,
            // Looting=105, Sitting=110, Crouching=111, Lying=115.
            // A transient combat swing (OP_Animation) overrides the looping animation for a short
            // window: action "C0{code}" resolves to the matching combat clip (C05 = 1H weapon, …).
            let action: String = match gs.combat_anims.get(&e.spawn_id) {
                Some((code, start)) if start.elapsed() < COMBAT_SWING_WINDOW => format!("C{:02}", code),
                _ => match e.animation {
                    100 => "idle",       // Animation::Standing
                    102 => "idle",       // Animation::Freeze
                    110 => "sitting",    // Animation::Sitting
                    111 => "crouching",  // Animation::Crouching
                    105 => "idle",       // Animation::Looting (treat as idle)
                    115 => "dead",       // Animation::Lying
                    _   => "idle",       // default / standing / safe default
                }.to_string(),
            };
            Billboard {
                id:        e.spawn_id,
                pos:       [e.x, e.y, e.z],
                level:     e.level,
                hp_pct:    e.hp_pct,
                is_target: gs.target_id == Some(e.spawn_id),
                dead:      e.dead,
                name:      e.name.clone(),
                race:      e.race.clone(),
                action:    action,
                heading:   e.heading,
                equipment:      e.equipment,
                equipment_tint: e.equipment_tint,
                gender:    e.gender,
            }
        }).collect();

        let messages = gs.messages.iter().map(|m| LogEntry {
            kind: m.kind.clone(),
            text: m.text.clone(),
            timestamp: m.timestamp,
        }).collect();

        SceneState {
            zone: gs.zone_name.clone(),
            zone_changed: gs.zone_changed,
            // World space is EQ native: [east=server_x, north=server_y, up=server_z].
            // Zone geometry, entities and the player all share this one frame.
            player_pos: [gs.player_x, gs.player_y, gs.player_z],
            player_heading: gs.player_heading,
            player_hp_pct: gs.hp_pct,
            player_mana_pct: gs.mana_pct,
            player_xp_pct: gs.xp_pct,
            player_name: gs.player_name.clone(),
            player_level: gs.player_level,
            player_race: gs.player_race.clone(),
            player_class: gs.player_class.clone(),
            player_gender: gs.player_gender,
            coin: gs.coin,
            stats: gs.stats,
            player_action: gs.player_action.clone(),
            target_name: gs.target_name.clone(),
            target_hp_pct: gs.target_hp_pct,
            target_con: gs.target_con,
            strategy: gs.strategy.clone(),
            billboards,
            messages,
            player_equipment: gs.player_equipment,
            player_equipment_tint: gs.player_equipment_tint,
            inventory: gs.inventory.clone(),
            primary_weapon_idfile: gs.inventory.iter().find(|i| i.slot == 13)
                .map(|i| i.idfile.clone()).unwrap_or_default(),
            secondary_weapon_idfile: gs.inventory.iter().find(|i| i.slot == 14)
                .map(|i| i.idfile.clone()).unwrap_or_default(),
            mem_spells: gs.mem_spells,
            // Drop a stale cast bar: if the cast time has elapsed plus a grace window and the
            // server never sent a terminal packet (OP_MemorizeSpell scribing=3 / OP_InterruptCast),
            // stop showing "Casting …" forever. (Spec Risks: cast_ms + grace fallback.)
            casting: gs.casting.clone().filter(|c| {
                c.started.elapsed().as_millis() < c.cast_ms as u128 + 1500
            }),
            sitting: gs.sitting,
            auto_attack: gs.auto_attack,
            target_id: gs.target_id,
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
    use super::SceneState;
    use crate::game_state::{Entity, GameState};

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
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0,
            animation: 0,
        });

        gs
    }

    #[test]
    fn from_game_state_sets_player_fields() {
        let scene = SceneState::from_game_state(&sample_state());
        assert_eq!(scene.player_name, "Aethas");
        assert_eq!(scene.player_pos, [1.0, 2.0, 3.0]); // EQ native [server_x, server_y, server_z]
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

    // --- Coordinate mapping: player_pos ---

    #[test]
    fn player_pos_coordinate_mapping() {
        let mut gs = GameState::new();
        gs.player_x = 100.0;
        gs.player_y = 200.0;
        gs.player_z = 50.0;
        let scene = SceneState::from_game_state(&gs);
        assert_eq!(
            scene.player_pos,
            [100.0, 200.0, 50.0],
            "player_pos is EQ native [server_x=east, server_y=north, server_z=up]"
        );
    }

    // --- Coordinate mapping: entity billboard pos ---

    #[test]
    fn billboard_pos_coordinate_mapping() {
        let mut gs = GameState::new();
        gs.upsert_entity(Entity {
            spawn_id: 1,
            name: "test".into(),
            level: 1,
            is_npc: true,
            x: 10.0,
            y: 20.0,
            z: 5.0,
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: String::new(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0,
            animation: 0,
        });
        let scene = SceneState::from_game_state(&gs);
        assert_eq!(scene.billboards.len(), 1);
        let b = &scene.billboards[0];
        assert_eq!(b.pos[0], 10.0, "pos[0] should be server_x (east)");
        assert_eq!(b.pos[1], 20.0, "pos[1] should be server_y (north)");
        assert_eq!(b.pos[2], 5.0,  "pos[2] should be server_z (height)");
    }

    // --- is_target flag ---

    #[test]
    fn target_entity_has_is_target_true() {
        let gs = sample_state(); // target_id = Some(42)
        let scene = SceneState::from_game_state(&gs);
        let targeted: Vec<_> = scene.billboards.iter().filter(|b| b.is_target).collect();
        assert_eq!(targeted.len(), 1);
        assert_eq!(targeted[0].id, 42);
    }

    #[test]
    fn non_target_entities_have_is_target_false() {
        let mut gs = sample_state();
        // Add a second entity that is NOT the target
        gs.upsert_entity(Entity {
            spawn_id: 99,
            name: "bystander".into(),
            level: 2,
            is_npc: true,
            x: 5.0, y: 5.0, z: 0.0,
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: String::new(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0,
            animation: 0,
        });
        gs.target_id = Some(42);
        let scene = SceneState::from_game_state(&gs);
        for b in &scene.billboards {
            if b.id == 42 {
                assert!(b.is_target, "id=42 should be targeted");
            } else {
                assert!(!b.is_target, "id={} should not be targeted", b.id);
            }
        }
    }

    #[test]
    fn from_game_state_propagates_equipment() {
        let mut gs = GameState::new();
        let mut e = Entity {
            spawn_id: 5, name: "x".into(), level: 1, is_npc: true,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 100.0, cur_hp: 1, max_hp: 1,
            race: "HUM".into(), heading: 0.0, dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0,
            animation: 0,
        };
        e.equipment[1] = 17;
        e.equipment_tint[1] = [9, 8, 7];
        gs.upsert_entity(e);
        let scene = SceneState::from_game_state(&gs);
        assert_eq!(scene.billboards[0].equipment[1], 17);
        assert_eq!(scene.billboards[0].equipment_tint[1], [9, 8, 7]);
    }

    #[test]
    fn from_game_state_propagates_gender() {
        let mut gs = GameState::new();
        gs.player_gender = 1; // female player
        let e = Entity {
            spawn_id: 5, name: "x".into(), level: 1, is_npc: true,
            x: 0.0, y: 0.0, z: 0.0, hp_pct: 100.0, cur_hp: 1, max_hp: 1,
            race: "HUM".into(), heading: 0.0, dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 1, helm: 0, showhelm: 0,
            animation: 0,
        };
        gs.upsert_entity(e);
        let scene = SceneState::from_game_state(&gs);
        assert_eq!(scene.billboards[0].gender, 1, "entity gender propagates to billboard");
        assert_eq!(scene.player_gender, 1, "player gender propagates to scene");
    }

    // --- Message count ---

    #[test]
    fn message_count_matches() {
        let mut gs = GameState::new();
        gs.log_msg("say", "hello");
        gs.log_msg("tell", "world");
        gs.log_msg("ooc", "third");
        let scene = SceneState::from_game_state(&gs);
        assert_eq!(scene.messages.len(), 3);
        assert_eq!(scene.messages[0].text, "hello");
        assert_eq!(scene.messages[2].text, "third");
    }
}
