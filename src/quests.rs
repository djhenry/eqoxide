//! Quest-giver data + the agent quest experience.
//!
//! EQEmu quests are Lua scripts (not in the DB), so `tools/quest_finder.py --export` bakes the
//! quest-giver info (location, wanted turn-in items, reward XP) into `quests.json`. That file is
//! delivered through the asset server's `gamedata` set (custom-content editable) and synced into the
//! local cache, from which this module loads it into a process-global and answers "is this NPC a
//! quest giver?" so the HUD can draw a golden "!" over them (like a modern MMO) and `GET /quests`
//! can list eligible quests near the player. See `docs/autonomous-play.md` §0 and the questing
//! roadmap in `todo.md`.

use std::collections::HashMap;
use std::sync::OnceLock;

/// One quest giver and what its quest needs/rewards (parsed from the server's Lua quest script).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct QuestGiver {
    pub npc_id: String,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    /// Turn-in items: (item_id, item_name, count) for the best (largest) turn-in tier.
    pub wanted: Vec<(u32, String, u32)>,
    /// Reward XP per turn-in tier (e.g. [14000, 28000]).
    pub reward_xp: Vec<u32>,
    /// The NPC's hail line (quest intro), if any.
    pub hail: String,
    /// True if this giver has an item turn-in (vs a pure dialogue quest).
    pub turn_in: bool,
}

/// zone short_name -> (clean NPC name -> giver). Clean name = spaces (matches `clean_entity_name`).
type QuestData = HashMap<String, HashMap<String, QuestGiver>>;
static QUESTS: OnceLock<QuestData> = OnceLock::new();

/// Load `quests.json` (synced from the asset server's `gamedata` set) once at startup. Missing/
/// invalid file = no quest indicators (the client still runs); regenerate with
/// `python3 tools/quest_finder.py --export`, which writes into the asset server's content dir.
pub fn load(path: &std::path::Path) {
    let data: QuestData = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let n: usize = data.values().map(|z| z.len()).sum();
    tracing::info!(
        "quests: loaded {} givers across {} zones from {}",
        n, data.len(), path.display()
    );
    let _ = QUESTS.set(data);
}

/// True if the (cleaned) NPC name has a quest in `zone` — used to draw the golden "!".
pub fn is_quest_giver(zone: &str, clean_name: &str) -> bool {
    QUESTS
        .get()
        .and_then(|d| d.get(zone))
        .map_or(false, |z| z.contains_key(clean_name))
}

/// Look up a single giver's quest detail.
pub fn quest_info(zone: &str, clean_name: &str) -> Option<QuestGiver> {
    QUESTS.get()?.get(zone)?.get(clean_name).cloned()
}

/// All quest givers known for a zone (for `GET /quests`).
pub fn givers_in(zone: &str) -> Vec<(String, QuestGiver)> {
    QUESTS
        .get()
        .and_then(|d| d.get(zone))
        .map(|z| z.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
        .unwrap_or_default()
}
