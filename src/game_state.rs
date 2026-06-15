//! In-game state — player, entities, zone info, message log.

use std::collections::VecDeque;
use crate::scene::LogEntry;

/// A zone exit point received in OP_SEND_ZONE_POINTS.
/// EQ wire format names are swapped: struct field `y` = server_x (north),
/// struct field `x` = server_y (east). We store in server convention.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ZonePoint {
    pub iterator:  u32,
    pub server_x:  f32,  // north  (wire field 'y')
    pub server_y:  f32,  // east   (wire field 'x')
    pub server_z:  f32,
    pub heading:   f32,
    pub zone_id:   u16,
}

/// A single entity in the zone (NPC or PC, not the player themselves).
#[derive(Debug, Clone)]
pub struct Entity {
    pub spawn_id: u32,
    pub name: String,
    pub level: u32,
    pub is_npc: bool,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub hp_pct: f32,
    pub cur_hp: i32,
    pub max_hp: i32,
    pub race: String,
    pub heading: f32,
    pub dead: bool,
}

impl Entity {
    pub fn dist_to(&self, x: f32, y: f32, z: f32) -> f32 {
        ((self.x - x).powi(2) + (self.y - y).powi(2) + (self.z - z).powi(2)).sqrt()
    }
}

/// All state the renderer needs for one frame.
#[derive(Debug, Default, Clone)]
pub struct GameState {
    // Player
    pub player_id: u32,
    pub player_name: String,
    pub player_x: f32,
    pub player_y: f32,
    pub player_z: f32,
    pub player_heading: f32,
    pub player_level: u32,
    pub player_race: String,
    pub player_class: String,
    pub player_action: String,
    pub hp_pct: f32,
    pub mana_pct: f32,
    pub xp_pct: f32,
    /// Coin on hand (platinum, gold, silver, copper), from the player profile.
    pub coin: [u32; 4],

    // Zone
    pub zone_name: String,
    pub zone_id: u16,
    pub zone_changed: bool,
    pub safe_x: f32,
    pub safe_y: f32,
    pub safe_z: f32,

    // Entities in zone (keyed by spawn_id)
    pub entities: std::collections::HashMap<u32, Entity>,

    // Target
    pub target_id: Option<u32>,
    pub target_name: Option<String>,
    pub target_hp_pct: Option<f32>,
    /// Consider color (RGB) of the current target, set from the OP_Consider reply.
    pub target_con: Option<[u8; 3]>,

    // Zone exit points (populated by OP_SEND_ZONE_POINTS on zone entry)
    pub zone_points: Vec<ZonePoint>,

    // Message log (ring buffer)
    pub messages: VecDeque<LogEntry>,

    // Strategy text for HUD
    pub strategy: String,
}

impl GameState {
    pub fn new() -> Self {
        GameState {
            messages: VecDeque::with_capacity(50),
            ..Default::default()
        }
    }

    pub fn log_msg(&mut self, kind: &str, text: &str) {
        if self.messages.len() >= 50 {
            self.messages.pop_front();
        }
        self.messages.push_back(LogEntry {
            kind: kind.to_string(),
            text: text.to_string(),
            timestamp: std::time::Instant::now(),
        });
    }

    pub fn upsert_entity(&mut self, e: Entity) {
        self.entities.insert(e.spawn_id, e);
    }

    pub fn remove_entity(&mut self, spawn_id: u32) {
        self.entities.remove(&spawn_id);
        if self.target_id == Some(spawn_id) {
            self.target_id = None;
        }
    }

    pub fn update_hp(&mut self, spawn_id: u32, cur_hp: i32, max_hp: i32) {
        if spawn_id == self.player_id {
            self.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
        } else if let Some(e) = self.entities.get_mut(&spawn_id) {
            e.cur_hp = cur_hp;
            e.max_hp = max_hp;
            e.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
        }
    }

    pub fn nearby_npcs(&self, max_dist: f32) -> Vec<&Entity> {
        let mut result: Vec<&Entity> = self
            .entities
            .values()
            .filter(|e| {
                e.is_npc
                    && !e.dead
                    && !e.name.contains("'s corpse")
                    && e.dist_to(self.player_x, self.player_y, self.player_z) <= max_dist
            })
            .collect();
        result.sort_by(|a, b| {
            let da = a.dist_to(self.player_x, self.player_y, self.player_z);
            let db = b.dist_to(self.player_x, self.player_y, self.player_z);
            da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
        });
        result
    }
}
