//! In-game state — player, entities, zone info, message log.

use std::collections::VecDeque;
use crate::scene::LogEntry;

/// A zone exit point received in OP_SEND_ZONE_POINTS.
/// Stored in EQ server convention: server_x = east, server_y = north, server_z = up.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ZonePoint {
    pub iterator:  u32,
    pub server_x:  f32,  // east  (wire field 'x')
    pub server_y:  f32,  // north (wire field 'y')
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
    #[allow(dead_code)]
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
    pub equipment: [u32; 9],
    pub equipment_tint: [[u8; 3]; 9],
    pub gender: u8,
    pub helm: u8,
    pub showhelm: u8,
    /// Face variant (0-indexed from Spawn_Struct `face`).  The rendered face primitive
    /// has `eq_part_index == face + 1`.
    pub face: u8,
    /// Hair style (from Spawn_Struct `hairstyle`).  0 = bald.  Rendered hair primitive
    /// has `eq_part_index == hairstyle` (when > 0).
    pub hairstyle: u8,
    /// Server animation state (Animation::Standing=100, Sitting=110, Crouching=111, etc.)
    pub animation: u32,
}

impl Entity {
    #[allow(dead_code)]
    pub fn dist_to(&self, x: f32, y: f32, z: f32) -> f32 {
        ((self.x - x).powi(2) + (self.y - y).powi(2) + (self.z - z).powi(2)).sqrt()
    }
}

/// A zone door (from OP_SpawnDoor). Position is stored in client convention
/// (x = east, y = north, z = up), converted from the wire's y-first order.
#[derive(Debug, Clone)]
pub struct Door {
    pub door_id: u8,
    pub name: String,        // model name, e.g. "DOOR1"
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub heading: f32,        // EQ heading (0..512)
    pub incline: i32,
    pub size: u16,           // 100 = normal scale
    pub opentype: u8,
    pub door_param: u32,
    pub invert_state: bool,  // true = normally-open door
    pub is_open: bool,       // authoritative, from server
    pub open_frac: f32,      // render-only: eases 0..1 toward is_open
}

/// One objective/step of a Task-system quest (from OP_TaskActivity). `done_count`/`goal_count`
/// are the live progress (e.g. "kill 4 gnolls" -> goal 4, done 2).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct TaskActivity {
    pub activity_id:   u32,
    pub activity_type: u32,
    /// The objective text — activity_name if present, else the mob/item the step targets.
    pub target:        String,
    pub done_count:    u32,
    pub goal_count:    u32,
}

/// A Task-system quest in the player's journal (from OP_TaskDescription + OP_TaskActivity). This is
/// EQ's *native* quest log (server-pushed), distinct from the old-style Lua turn-in quests surfaced
/// by tools/quest_finder.py + GET /quests. See docs/autonomous-play.md.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ActiveTask {
    pub task_id:     u32,
    pub title:       String,
    pub description: String,
    pub xp_reward:   u32,
    pub coin_reward: u32,
    pub activities:  Vec<TaskActivity>,
}

/// One item in the player's inventory/equipment (decoded from OP_CharInventory / OP_ItemPacket).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct InvItem {
    /// RoF2 wire slot id: equipment 0-22, general-inventory 23-32, cursor 33 (rof2_limits.h).
    /// Stored as-is from the server's OP_CharInventory / OP_ItemPacket main_slot field so that
    /// client→server packets (MoveItem, Merchant_Purchase) can send the same value back.
    pub slot:    i32,
    pub item_id: u32,
    pub name:    String,
    pub icon:    u32,
    /// Stack quantity / charges (1 for non-stackable).
    pub charges: i32,
    /// EQ IDFile (e.g. "IT63") — the held/world model id, used to render the weapon in hand.
    pub idfile:  String,
}

/// One item offered by an open merchant (decoded from OP_ItemPacket with PacketType=Merchant,
/// sent by the server after a successful OP_ShopRequest). Drives `GET /trade/list` + the HUD
/// merchant window. `merchant_slot` is the slot to pass to `POST /trade/buy`.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MerchantItem {
    pub merchant_slot: u32,
    pub item_id: u32,
    pub name:    String,
    pub icon:    u32,
    pub price:   u32,
    /// Quantity the merchant stocks (-1 / large = effectively unlimited).
    pub quantity: i32,
}

/// Active spell-cast in progress.
#[derive(Debug, Clone)]
pub struct CastState {
    pub spell_id: u32,
    pub started: std::time::Instant,
    pub cast_ms: u32,
}

/// One async game event the agent should know about as soon as it happens — surfaced via the
/// `/v1/events/*` feed. `category` is the top-level bucket the events API filters on
/// ("chat" | "combat" | "navigate" | "system"); `kind` is the sub-type within it (e.g. chat →
/// tell/ooc/shout/group/gmsay, navigate → zone, combat → slain/attacked). `directed` = addressed
/// specifically to us (a /tell to our name, a GM message, or something happening to *us*). `id` is
/// monotonic (1-based) per session so an agent can poll `?since=<id>` without missing or re-seeing
/// events. NPC dialogue (say channel) is NOT recorded here — it stays in `messages`.
#[derive(Debug, Default, Clone)]
pub struct ChatLogEvent {
    pub id:       u64,
    pub category: String,  // "chat" | "combat" | "navigate" | "system"
    pub kind:     String,  // sub-type, e.g. "tell"/"ooc"/"zone"/"slain"/"attacked"
    pub from:     String,
    pub directed: bool,
    pub text:     String,
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
    /// 0 = male, 1 = female (selects the gender model variant).
    pub player_gender: u8,
    /// Player face variant (0-indexed from PlayerProfile `face`, offset 00898).
    pub player_face: u8,
    /// Player hair style (from PlayerProfile `hairstyle`, offset 00896). 0 = bald.
    pub player_hairstyle: u8,
    pub player_action: String,
    pub hp_pct: f32,
    /// Player's absolute current/max HP (from OP_HP_UPDATE), used for the lethal-fall guard.
    pub cur_hp: i32,
    pub max_hp: i32,
    pub mana_pct: f32,
    /// Player's absolute current/max mana. Seeded from the PlayerProfile (no max in the profile, so
    /// max is seeded = cur at zone-in) and updated from OP_ManaChange, which carries only the new
    /// current mana — so `max_mana` is a high-water-mark (accurate once the char has been at full
    /// mana, i.e. immediately at zone-in for a rested caster). See `set_mana`. (eqoxide#27)
    pub cur_mana: i32,
    pub max_mana: i32,
    pub xp_pct: f32,
    /// Coin on hand (platinum, gold, silver, copper), from the player profile.
    pub coin: [u32; 4],
    /// Stats (STR, STA, CHA, DEX, INT, AGI, WIS), from the player profile.
    pub stats: [u32; 7],
    /// Item material IDs for each equipment slot (0..9), from the player profile.
    pub player_equipment: [u32; 9],
    /// RGB tint for each equipment slot (0..9), from the player profile.
    pub player_equipment_tint: [[u8; 3]; 9],
    /// Transient one-shot combat swings, keyed by spawn_id (player uses gs.player_id): the EQ
    /// animation action code (1=kick … 5=1H weapon … 8=hand-to-hand) + when it started. Set from
    /// OP_Animation; the renderer plays clip C0{action} for a short window, then reverts to idle/walk.
    pub combat_anims: std::collections::HashMap<u32, (u8, std::time::Instant)>,

    // Zone
    pub zone_name: String,
    pub zone_id: u16,
    pub zone_changed: bool,
    /// Zone id the server told us to move to (OP_RequestClientZoneChange, e.g. a portal
    /// door). The nav thread drains this and initiates the normal zone-change handshake.
    pub pending_server_zone: Option<u16>,
    pub safe_x: f32,
    pub safe_y: f32,
    pub safe_z: f32,

    // Entities in zone (keyed by spawn_id)
    pub entities: std::collections::HashMap<u32, Entity>,

    // Doors in zone (keyed by per-zone door_id), from OP_SpawnDoor.
    pub doors: std::collections::HashMap<u8, Door>,

    // Target
    pub target_id: Option<u32>,
    pub target_name: Option<String>,
    /// NPCs that have recently swung at the player (hit or miss), keyed by spawn id → time of the
    /// last swing. Auto-combat uses this to engage an add that aggros mid-fight instead of letting
    /// it beat the player unanswered, while keeping the current target if it is also attacking us
    /// (so two adds don't cause target thrash). Set in `apply_combat_damage`; read + pruned by the
    /// nav auto-retarget.
    pub recent_attackers: std::collections::HashMap<u32, std::time::Instant>,
    pub target_hp_pct: Option<f32>,
    /// Consider color (RGB) of the current target, set from the OP_Consider reply.
    pub target_con: Option<[u8; 3]>,

    // Zone exit points (populated by OP_SEND_ZONE_POINTS on zone entry)
    pub zone_points: Vec<ZonePoint>,

    // Message log (ring buffer)
    pub messages: VecDeque<LogEntry>,

    // Inter-agent chat events (tells/ooc/shout/group/gmsay) for the GET /events feed.
    pub chat_events:  VecDeque<ChatLogEvent>,
    pub next_chat_id: u64,

    // UCS (chat server) connection params from OP_SetChatServer; Some once received at zone-in.
    pub ucs: Option<crate::eq_net::ucs::UcsInfo>,

    // Strategy text for HUD
    pub strategy: String,

    /// Count of server rubber-band corrections (position deltas > 5 units).
    pub server_corrections: u32,

    // Loot state
    /// Corpse spawn_ids queued for auto-looting (populated by OP_BecomeCorpse).
    pub pending_loot: VecDeque<u32>,
    /// True while a loot session is open (LootRequest sent, waiting for server items).
    pub loot_session_active: bool,
    /// Updated each time the server sends a loot packet; used to time out the session.
    pub loot_last_activity: Option<std::time::Instant>,
    /// When the first corpse was pushed to pending_loot; used to delay LootRequest by
    /// 500 ms so the server has time to register the corpse as lootable.
    pub loot_queued_at: Option<std::time::Instant>,

    // Quest log (native EQ Task system) — server-pushed via OP_TaskDescription / OP_TaskActivity.
    /// Active task quests keyed by task_id, with their objectives + live progress.
    pub tasks: std::collections::HashMap<u32, ActiveTask>,
    /// Task ids the server reports as completed (OP_CompletedTasks).
    pub completed_tasks: Vec<u32>,

    /// Player inventory + equipment (decoded from OP_CharInventory / OP_ItemPacket).
    pub inventory: Vec<InvItem>,

    /// Set true when the server sends OP_TradeRequestAck — the trade session now exists, so the
    /// nav thread may move the cursor item into the NPC trade slot and accept. Cleared once the
    /// give state machine consumes it (or on timeout). See navigation.rs.
    pub trade_ack_ready: bool,

    // Spellcasting / posture
    /// Memorized spell gem IDs (9 slots); 0xFFFF_FFFF = empty slot.
    pub mem_spells: [u32; 9],
    /// Active cast in progress (Some) or idle (None).
    pub casting: Option<CastState>,
    /// True when the player is sitting.
    pub sitting: bool,
    /// True when auto-attack is enabled.
    pub auto_attack: bool,

    /// Spawn id of the player's own pet (a spawn whose petOwnerId == player_id, e.g. a summoned
    /// necro pet), or None when she has no pet. Drives OP_PetCommands + auto-pet-combat.
    pub pet_id: Option<u32>,

    // Merchant / trade session
    /// `Some(merchant_entity_id)` while a merchant window is open (server accepted OP_ShopRequest
    /// with command=Open); `None` when closed or the server rejected it (command=Close, e.g. KOS
    /// faction). Drives the HUD merchant window's visibility + `GET /trade/list` `open` flag.
    pub merchant_open: Option<u32>,
    /// Items the open merchant offers (cleared on close). From OP_ItemPacket(PacketType=Merchant).
    pub merchant_items: Vec<MerchantItem>,
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

    /// Record an inter-agent chat event (tell/ooc/shout/group/gmsay) for the GET /events feed,
    /// assigning the next monotonic id. Capped to the most recent 200 events.
    /// Record an async event onto the `/v1/events/*` feed. `category` is the top-level bucket
    /// (chat/combat/navigate/system); `kind` the sub-type; `from` the originator ("" / "system" for
    /// non-player events); `directed` whether it concerns us specifically.
    pub fn push_event(&mut self, category: &str, kind: &str, from: &str, directed: bool, text: &str) {
        // Ids are 1-based: the events endpoint filters `id > since` with `since=0` as the default
        // "haven't seen anything" cursor, so a 0-id first event would be permanently invisible.
        self.next_chat_id += 1;
        let id = self.next_chat_id;
        if self.chat_events.len() >= 200 {
            self.chat_events.pop_front();
        }
        self.chat_events.push_back(ChatLogEvent {
            id,
            category: category.to_string(),
            kind: kind.to_string(),
            from: from.to_string(),
            directed,
            text: text.to_string(),
        });
    }

    pub fn upsert_entity(&mut self, e: Entity) {
        self.entities.insert(e.spawn_id, e);
    }

    /// Deduct `copper` from on-hand coin and redistribute the remaining total into
    /// platinum/gold/silver/copper (1pp=10gp=100sp=1000cp). Returns false (no change) if funds are
    /// insufficient. Used for merchant buys, which the server takes client-side (update_client=false)
    /// without sending an OP_MoneyUpdate — so the HUD coin would otherwise stay stale.
    pub fn spend_coin(&mut self, copper: u64) -> bool {
        let total = self.coin[0] as u64 * 1000 + self.coin[1] as u64 * 100
                  + self.coin[2] as u64 * 10  + self.coin[3] as u64;
        if copper > total { return false; }
        let r = total - copper;
        self.coin = [(r / 1000) as u32, ((r % 1000) / 100) as u32, ((r % 100) / 10) as u32, (r % 10) as u32];
        true
    }

    /// Mirror a client-authoritative whole-item move (OP_MoveItem) into the local snapshot.
    /// EQEmu applies inventory moves silently — it validates the client's OP_MoveItem and updates
    /// the server inventory but sends no echo (the real client already moved the item in its own
    /// UI). eqoxide has no such UI, so it must apply the move to `gs.inventory` itself or the
    /// `/inventory` view goes stale (and a later move computed against the stale view corrupts it).
    /// If `to` is occupied the two items swap slots (matches EQEmu SwapItem); moving from an empty
    /// slot is a no-op. `from`/`to` are RoF2 wire slots, the same space `gs.inventory` is keyed on.
    pub fn move_item(&mut self, from: i32, to: i32) {
        if from == to { return; }
        let Some(from_idx) = self.inventory.iter().position(|i| i.slot == from) else { return; };
        if let Some(to_idx) = self.inventory.iter().position(|i| i.slot == to) {
            self.inventory[to_idx].slot = from; // occupied destination → swap
        }
        self.inventory[from_idx].slot = to;
    }

    /// Drop any items still sitting in the NPC trade slots (RoF2 3000-3007). On a quest turn-in the
    /// server takes the handed-in items via `m_inv.PopItem` (zone/trading.cpp) with no client
    /// packet, so once the trade completes the client must clear its own trade slots. Items the NPC
    /// returns (or rewards) come back separately as OP_ItemPacket on the cursor.
    pub fn clear_trade_slots(&mut self) {
        self.inventory.retain(|i| !(3000..=3007).contains(&i.slot));
    }

    pub fn remove_entity(&mut self, spawn_id: u32) {
        self.entities.remove(&spawn_id);
        if self.target_id == Some(spawn_id) {
            self.target_id = None;
        }
        if self.pet_id == Some(spawn_id) {
            self.pet_id = None; // pet died / despawned
        }
    }

    pub fn upsert_door(&mut self, d: Door) {
        self.doors.insert(d.door_id, d);
    }

    /// Apply a server door-state change. Unknown door ids are ignored.
    pub fn set_door_open(&mut self, door_id: u8, open: bool) {
        if let Some(d) = self.doors.get_mut(&door_id) {
            d.is_open = open;
        }
    }

    pub fn update_hp(&mut self, spawn_id: u32, cur_hp: i32, max_hp: i32) {
        if spawn_id == self.player_id {
            self.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
            self.cur_hp = cur_hp;
            self.max_hp = max_hp;
        } else if let Some(e) = self.entities.get_mut(&spawn_id) {
            e.cur_hp = cur_hp;
            e.max_hp = max_hp;
            e.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
        }
    }

    /// Apply a percent-only HP update (OP_MobHealth / `SpawnHPUpdate_Struct2`). A mob
    /// you are fighting but not grouped with only sends its HP as a 0-100 percentage,
    /// so there is no absolute cur/max to record — just its `hp_pct`. The target HUD
    /// readout (`target_hp_pct`) follows `entities[id].hp_pct`, so this is what makes a
    /// fought mob's health bar move. Don't touch the player's own bar here: the player
    /// gets a full OP_HPUpdate with real cur/max, which is strictly better. (eqoxide#51)
    pub fn update_hp_pct(&mut self, spawn_id: u32, hp_pct: f32) {
        if spawn_id != self.player_id {
            if let Some(e) = self.entities.get_mut(&spawn_id) {
                e.hp_pct = hp_pct;
            }
        }
    }

    /// Set `xp_pct` from an OP_ExpUpdate `exp` field, a 0-330 ratio of progress
    /// through the current level. Convert to a 0-100 percentage and clamp (a
    /// freshly-leveled character can momentarily report slightly over 330). (eqoxide#48)
    pub fn set_xp(&mut self, exp_ratio: u32) {
        self.xp_pct = (exp_ratio as f32 / 330.0 * 100.0).clamp(0.0, 100.0);
    }

    /// Set the player's current mana and recompute `mana_pct`. The mana wire (PlayerProfile seed,
    /// OP_ManaChange) carries only the *current* mana — there is no max in either — so `max_mana`
    /// is tracked as a high-water-mark: it grows to the largest current mana seen. At zone-in a
    /// rested caster is at full mana, so the seed sets the correct max; spending then lowers the
    /// percent. (eqoxide#27)
    pub fn set_mana(&mut self, cur_mana: i32) {
        self.cur_mana = cur_mana;
        if cur_mana > self.max_mana { self.max_mana = cur_mana; }
        self.mana_pct = (cur_mana as f32 / self.max_mana.max(1) as f32) * 100.0;
    }

    #[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::{Door, Entity, GameState};

    fn make_entity(id: u32, name: &str, x: f32, y: f32, z: f32, is_npc: bool) -> Entity {
        Entity {
            spawn_id: id,
            name: name.to_string(),
            level: 1,
            is_npc,
            x,
            y,
            z,
            hp_pct: 100.0,
            cur_hp: 100,
            max_hp: 100,
            race: String::new(),
            heading: 0.0,
            dead: false,
            equipment: [0; 9], equipment_tint: [[0; 3]; 9], gender: 0, helm: 0, showhelm: 0,
            face: 0, hairstyle: 0,
            animation: 0,
        }
    }

    // --- Entity::dist_to ---

    #[test]
    fn dist_to_3_4_0_gives_5() {
        let e = make_entity(1, "mob", 3.0, 4.0, 0.0, true);
        let d = e.dist_to(0.0, 0.0, 0.0);
        assert!((d - 5.0).abs() < 1e-5, "expected 5.0, got {d}");
    }

    #[test]
    fn dist_to_same_position_is_zero() {
        let e = make_entity(1, "mob", 7.0, 8.0, 9.0, true);
        let d = e.dist_to(7.0, 8.0, 9.0);
        assert!((d - 0.0).abs() < 1e-5, "expected 0.0, got {d}");
    }

    // --- GameState::log_msg ---

    #[test]
    fn log_msg_preserves_kind_and_text() {
        let mut gs = GameState::new();
        gs.log_msg("chat", "hello world");
        assert_eq!(gs.messages.len(), 1);
        assert_eq!(gs.messages[0].kind, "chat");
        assert_eq!(gs.messages[0].text, "hello world");
    }

    #[test]
    fn spend_coin_redistributes_and_guards_funds() {
        let mut gs = GameState::new();
        gs.coin = [84, 9, 13, 8]; // = 84*1000 + 9*100 + 13*10 + 8 = 85038 copper
        // Spend 1c -> 85037 -> 85p 0g 3s 7c (the unnormalized 13s gets consolidated)
        assert!(gs.spend_coin(1));
        assert_eq!(gs.coin, [85, 0, 3, 7]);
        // Spend a full plat (1000c) -> 84037 -> 84p 0g 3s 7c
        assert!(gs.spend_coin(1000));
        assert_eq!(gs.coin, [84, 0, 3, 7]);
        // Insufficient funds: no change, returns false
        assert!(!gs.spend_coin(10_000_000));
        assert_eq!(gs.coin, [84, 0, 3, 7]);
        // Spend everything (84037 copper)
        assert!(gs.spend_coin(84_037));
        assert_eq!(gs.coin, [0, 0, 0, 0]);
    }

    #[test]
    fn move_item_relocates_swaps_and_guards() {
        use super::InvItem;
        let mut gs = GameState::new();
        let mk = |slot: i32, id: u32| InvItem { slot, item_id: id, ..Default::default() };
        gs.inventory = vec![mk(24, 100), mk(17, 200)]; // bag slot 24 + worn chest 17

        // Move into an EMPTY slot relocates the item.
        gs.move_item(24, 30); // bag -> cursor (empty)
        assert_eq!(gs.inventory.iter().find(|i| i.item_id == 100).unwrap().slot, 30);
        assert!(gs.inventory.iter().all(|i| i.slot != 24), "source slot now empty");

        // Move into an OCCUPIED slot swaps the two items (EQEmu SwapItem semantics).
        gs.move_item(30, 17); // cursor item -> worn chest (occupied by id 200)
        assert_eq!(gs.inventory.iter().find(|i| i.item_id == 100).unwrap().slot, 17);
        assert_eq!(gs.inventory.iter().find(|i| i.item_id == 200).unwrap().slot, 30);
        assert_eq!(gs.inventory.len(), 2, "swap must not create or drop items");

        // Move FROM an empty slot is a no-op.
        gs.move_item(99, 23);
        assert_eq!(gs.inventory.len(), 2);
        assert!(gs.inventory.iter().all(|i| i.slot != 23));
    }

    #[test]
    fn clear_trade_slots_removes_handed_in_items() {
        use super::InvItem;
        let mut gs = GameState::new();
        let mk = |slot: i32, id: u32| InvItem { slot, item_id: id, ..Default::default() };
        // Two items sitting in NPC trade slots (handed in) + one normal bag item.
        gs.inventory = vec![mk(3000, 100), mk(3001, 101), mk(24, 200)];
        gs.clear_trade_slots();
        assert_eq!(gs.inventory.len(), 1, "both trade-slot items consumed");
        assert_eq!(gs.inventory[0].slot, 24, "non-trade item untouched");
    }

    #[test]
    fn log_msg_drops_oldest_when_full() {
        let mut gs = GameState::new();
        // Fill to exactly 50
        for i in 0..50 {
            gs.log_msg("kind", &format!("msg {i}"));
        }
        assert_eq!(gs.messages.len(), 50);
        assert_eq!(gs.messages[0].text, "msg 0");

        // Adding one more should drop "msg 0"
        gs.log_msg("kind", "msg 50");
        assert_eq!(gs.messages.len(), 50);
        assert_eq!(gs.messages[0].text, "msg 1");
        assert_eq!(gs.messages[49].text, "msg 50");
    }

    // --- GameState::upsert_entity / remove_entity ---

    #[test]
    fn upsert_then_remove_entity_gone() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(10, "goblin", 0.0, 0.0, 0.0, true));
        assert!(gs.entities.contains_key(&10));
        gs.remove_entity(10);
        assert!(!gs.entities.contains_key(&10));
    }

    #[test]
    fn remove_entity_clears_target_id() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(10, "goblin", 0.0, 0.0, 0.0, true));
        gs.target_id = Some(10);
        gs.remove_entity(10);
        assert_eq!(gs.target_id, None);
    }

    #[test]
    fn remove_entity_leaves_other_target_intact() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(10, "goblin", 0.0, 0.0, 0.0, true));
        gs.upsert_entity(make_entity(11, "orc", 1.0, 0.0, 0.0, true));
        gs.target_id = Some(11);
        gs.remove_entity(10);
        assert_eq!(gs.target_id, Some(11));
    }

    #[test]
    fn upsert_overwrites_by_spawn_id() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(5, "original", 0.0, 0.0, 0.0, true));
        gs.upsert_entity(make_entity(5, "updated", 1.0, 2.0, 3.0, true));
        assert_eq!(gs.entities.len(), 1);
        assert_eq!(gs.entities[&5].name, "updated");
    }

    // --- GameState::update_hp ---

    #[test]
    fn update_hp_player_sets_hp_pct() {
        let mut gs = GameState::new();
        gs.player_id = 99;
        gs.update_hp(99, 75, 100);
        assert!((gs.hp_pct - 75.0).abs() < 1e-4, "expected 75.0, got {}", gs.hp_pct);
    }

    #[test]
    fn set_mana_seeds_max_then_tracks_spending() {
        let mut gs = GameState::new();
        // First call (seed at zone-in, full mana): max grows from 0 to cur → 100%.
        gs.set_mana(500);
        assert_eq!(gs.cur_mana, 500);
        assert_eq!(gs.max_mana, 500, "max seeded from first (full) value");
        assert!((gs.mana_pct - 100.0).abs() < 1e-4);
        // Spending lowers cur, max held → percent drops.
        gs.set_mana(200);
        assert_eq!(gs.max_mana, 500, "spending must not lower the high-water max");
        assert!((gs.mana_pct - 40.0).abs() < 1e-4, "200/500 = 40%, got {}", gs.mana_pct);
        // Regen above the prior max grows the high-water mark (e.g. seeded while not full).
        gs.set_mana(600);
        assert_eq!(gs.max_mana, 600);
        assert!((gs.mana_pct - 100.0).abs() < 1e-4);
    }

    #[test]
    fn update_hp_entity_sets_hp_pct() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "mob", 0.0, 0.0, 0.0, true));
        gs.update_hp(7, 50, 200);
        let e = &gs.entities[&7];
        assert_eq!(e.cur_hp, 50);
        assert_eq!(e.max_hp, 200);
        assert!((e.hp_pct - 25.0).abs() < 1e-4, "expected 25.0, got {}", e.hp_pct);
    }

    #[test]
    fn update_hp_pct_sets_entity_percent_only() {
        // OP_MobHealth carries only a 0-100 percentage: hp_pct moves, cur/max untouched.
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "mob", 0.0, 0.0, 0.0, true));
        gs.update_hp(7, 50, 200); // seed cur/max via a full update first
        gs.update_hp_pct(7, 40.0);
        let e = &gs.entities[&7];
        assert!((e.hp_pct - 40.0).abs() < 1e-4, "expected 40.0, got {}", e.hp_pct);
        assert_eq!(e.cur_hp, 50, "percent-only update must not touch cur_hp");
        assert_eq!(e.max_hp, 200, "percent-only update must not touch max_hp");
    }

    #[test]
    fn update_hp_pct_ignores_player_self() {
        // The player has a better full OP_HPUpdate path; a percent-only update must not
        // clobber the player's own bar.
        let mut gs = GameState::new();
        gs.player_id = 1;
        gs.hp_pct = 88.0;
        gs.update_hp_pct(1, 5.0);
        assert!((gs.hp_pct - 88.0).abs() < 1e-4, "player hp_pct must be untouched");
    }

    #[test]
    fn update_hp_max_zero_does_not_panic() {
        let mut gs = GameState::new();
        gs.player_id = 1;
        // max_hp=0 → uses max(1) guard; cur_hp=0 → 0%
        gs.update_hp(1, 0, 0);
        assert!((gs.hp_pct - 0.0).abs() < 1e-4);
    }

    #[test]
    fn set_xp_converts_330_ratio_to_percent() {
        let mut gs = GameState::new();
        gs.set_xp(0);
        assert!((gs.xp_pct - 0.0).abs() < 1e-4);
        gs.set_xp(165); // half-way through the level
        assert!((gs.xp_pct - 50.0).abs() < 1e-3, "expected 50.0, got {}", gs.xp_pct);
        gs.set_xp(330); // full → clamps to 100
        assert!((gs.xp_pct - 100.0).abs() < 1e-4);
        gs.set_xp(400); // over-range guard
        assert!((gs.xp_pct - 100.0).abs() < 1e-4);
    }

    // --- GameState::nearby_npcs ---

    #[test]
    fn nearby_npcs_sorted_nearest_first() {
        let mut gs = GameState::new();
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.player_z = 0.0;
        // dist = 5.0
        gs.upsert_entity(make_entity(1, "far", 3.0, 4.0, 0.0, true));
        // dist = 1.0
        gs.upsert_entity(make_entity(2, "near", 1.0, 0.0, 0.0, true));
        let npcs = gs.nearby_npcs(100.0);
        assert_eq!(npcs.len(), 2);
        assert_eq!(npcs[0].spawn_id, 2, "nearest should be id=2");
        assert_eq!(npcs[1].spawn_id, 1, "farthest should be id=1");
    }

    #[test]
    fn nearby_npcs_excludes_dead() {
        let mut gs = GameState::new();
        let mut dead = make_entity(1, "zombie", 0.0, 0.0, 0.0, true);
        dead.dead = true;
        gs.upsert_entity(dead);
        assert!(gs.nearby_npcs(100.0).is_empty());
    }

    #[test]
    fn nearby_npcs_excludes_corpses() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(1, "goblin's corpse", 0.0, 0.0, 0.0, true));
        assert!(gs.nearby_npcs(100.0).is_empty());
    }

    #[test]
    fn nearby_npcs_excludes_pcs() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(1, "Playerone", 0.0, 0.0, 0.0, false));
        assert!(gs.nearby_npcs(100.0).is_empty());
    }

    #[test]
    fn nearby_npcs_excludes_beyond_max_dist() {
        let mut gs = GameState::new();
        gs.player_x = 0.0;
        gs.player_y = 0.0;
        gs.player_z = 0.0;
        // dist = 10.0, max_dist = 5.0 → excluded
        gs.upsert_entity(make_entity(1, "faraway", 10.0, 0.0, 0.0, true));
        assert!(gs.nearby_npcs(5.0).is_empty());
    }

    // --- Door state management ---

    #[test]
    fn door_open_state_round_trips() {
        let mut gs = GameState::new();
        gs.upsert_door(Door {
            door_id: 3, name: "DOOR1".into(),
            x: 10.0, y: 20.0, z: 5.0, heading: 0.0, incline: 0, size: 100,
            opentype: 5, door_param: 0, invert_state: false,
            is_open: false, open_frac: 0.0,
        });
        gs.set_door_open(3, true);
        assert!(gs.doors.get(&3).unwrap().is_open);
        gs.set_door_open(3, false);
        assert!(!gs.doors.get(&3).unwrap().is_open);
        // Unknown door id is ignored, not a panic.
        gs.set_door_open(99, true);
        assert!(gs.doors.get(&99).is_none());
    }
}
