//! In-game state — player, entities, zone info, message log.

use std::collections::VecDeque;
use crate::scene::LogEntry;

/// A zone exit point received in OP_SEND_ZONE_POINTS.
/// Stored in EQ server convention: server_x = east, server_y = north, server_z = up.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ZonePoint {
    pub iterator:  u32,
    pub server_x:  f32,  // east  (wire field 'x')
    pub server_y:  f32,  // north (wire field 'y')
    pub server_z:  f32,
    pub heading:   f32,
    pub zone_id:   u16,
}

/// A single entity in the zone (NPC or PC, not the player themselves).
#[derive(Debug, Clone, PartialEq)]
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
    /// Hair color index (Spawn_Struct `haircolor`, 0-23). Runtime-tints synthetic hair shells only.
    pub haircolor: u8,
    /// Server animation state (Animation::Standing=100, Sitting=110, Crouching=111, etc.)
    pub animation: u32,
    /// True for boat/ship races: they float on the water surface and are exempt from the render
    /// floor-snap (matching the server's `Mob::FixZ` boat skip) so they don't sink (#194).
    pub floating: bool,
}

impl Entity {
    #[allow(dead_code)]
    pub fn dist_to(&self, x: f32, y: f32, z: f32) -> f32 {
        ((self.x - x).powi(2) + (self.y - y).powi(2) + (self.z - z).powi(2)).sqrt()
    }
}

/// A zone door (from OP_SpawnDoor). Position is stored in client convention
/// (x = east, y = north, z = up), converted from the wire's y-first order.
#[derive(Debug, Clone, PartialEq)]
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
}

/// Zone distance fog parameters (RoF2 `NewZone_Struct` slot 0), see `GameState::zone_fog`.
/// `color` is 0-255 RGB, matching the wire's `uint8 fog_red/green/blue[4]`; `minclip`/`maxclip`
/// are the linear fog-fade distance range; `density` is a 0..1 blend-intensity cap applied on top
/// of the linear fade (NOT a D3DFOG_EXP density — see the field's doc comment on `NewZone_S`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneFog {
    pub color:   [u8; 3],
    pub minclip: f32,
    pub maxclip: f32,
    pub density: f32,
}

/// One objective/step of a Task-system quest (from OP_TaskActivity). `done_count`/`goal_count`
/// are the live progress (e.g. "kill 4 gnolls" -> goal 4, done 2).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TaskActivity {
    pub activity_id:   u32,
    pub activity_type: u32,
    /// The objective text — activity_name if present, else the mob/item the step targets.
    pub target:        String,
    pub done_count:    u32,
    pub goal_count:    u32,
}

/// Lifecycle state of a Task-system quest, from `OP_TaskDescription`'s implicit "active" arrival
/// or `OP_CompletedTasks`'/`OP_CancelTask`'s explicit signal. Defaults to Active because a task
/// only exists in `gs.tasks` once the server has told us about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
pub enum TaskStatus {
    #[default]
    Active,
    Completed,
    Cancelled,
}

/// A Task-system quest in the player's journal (from OP_TaskDescription + OP_TaskActivity). This is
/// EQ's *native* quest log (server-pushed) — the same journal a human sees in their own quest
/// window, surfaced by GET /v1/quests/log. See docs/autonomous-play.md.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ActiveTask {
    pub task_id:     u32,
    pub title:       String,
    pub description: String,
    pub xp_reward:   u32,
    pub coin_reward: u32,
    /// Reward item name, parsed from OP_TaskDescription's item_link cstr (EQ saylink markup
    /// stripped). Empty if the task has no item reward.
    pub reward_item_text: String,
    pub status: TaskStatus,
    /// The journal display-order slot EQEmu calls `SequenceNumber` (0 = first task, 1 = second,
    /// ...). `OP_CancelTask` addresses a task by this, not by task_id — see `TaskStatus`.
    pub sequence_number: u32,
    pub activities:  Vec<TaskActivity>,
}

/// One task offered by an open task-selector window (from `OP_TaskSelectWindow`, sent when an NPC
/// script calls `tasksetselector` instead of auto-granting via `assigntask`). No content on this
/// server's live scripts uses the selector path today, but the protocol path is real.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct TaskOffer {
    pub task_id: u32,
    /// The offering NPC's entity id — required by `OP_AcceptNewTask`'s `task_master_id` field.
    pub npc_id: u32,
    pub title: String,
    pub description: String,
    /// Whether the task has rewards. No numeric/text reward info exists at offer time — only
    /// `OP_TaskDescription` (sent after acceptance) carries the actual reward amounts.
    pub has_rewards: bool,
}

/// One entry from the player's completed-task history (`OP_CompletedTasks`).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CompletedTaskEntry {
    pub task_id: u32,
    pub title: String,
    /// Unix time the task was completed, as sent by the server.
    pub completed_time: u32,
}

/// One item in the player's inventory/equipment (decoded from OP_CharInventory / OP_ItemPacket).
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
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
    /// Item's click ("clicky") spell id (0 = none). Lets an agent activate a teleport ring / port
    /// potion via `POST /v1/combat/cast {"item_slot": <this item's slot>}`. (eqoxide#193)
    #[serde(default)]
    pub click_spell_id: u32,
    /// Book/note text-file id (`Item.Filename`). Empty for non-books; when set, the item is READABLE
    /// via `POST /v1/interact/read {"slot":N}` → the server returns the text (#288).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub filename: String,
}

/// First flat bag-content wire slot (RoF2 invbag::GENERAL_BAGS_BEGIN). A container in general slot
/// `p` (23-32) exposes its 10 sub-slots at `251 + (p-23)*10 + sub` for `sub` in 0..9. (eqoxide#201)
pub const BAG_SLOTS_BEGIN: i32 = 251;

/// Flat bag wire slot for a general-inventory container at `parent_slot` (23-32) holding a sub-item
/// at `sub_index` (0-9). None for non-general parents or out-of-range indices. (eqoxide#201)
pub fn bag_wire_slot(parent_slot: i32, sub_index: u32) -> Option<i32> {
    if (23..=32).contains(&parent_slot) && sub_index < 10 {
        Some(BAG_SLOTS_BEGIN + (parent_slot - 23) * 10 + sub_index as i32)
    } else {
        None
    }
}

/// Inverse of [`bag_wire_slot`]: the (parent general slot, sub-index) a flat bag slot decodes to,
/// or None if `flat` is not a general-bag content slot (251..=350). (eqoxide#201)
pub fn bag_wire_parent(flat: i32) -> Option<(i32, u32)> {
    if (BAG_SLOTS_BEGIN..=350).contains(&flat) {
        let o = flat - BAG_SLOTS_BEGIN;
        Some((23 + o / 10, (o % 10) as u32))
    } else {
        None
    }
}

/// One item offered by an open merchant (decoded from OP_ItemPacket with PacketType=Merchant,
/// sent by the server after a successful OP_ShopRequest). Drives `GET /trade/list` + the HUD
/// merchant window. `merchant_slot` is the slot to pass to `POST /trade/buy`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
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
#[derive(Debug, Clone, PartialEq)]
pub struct CastState {
    pub spell_id: u32,
    pub started: std::time::Instant,
    pub cast_ms: u32,
}

/// Sentinel for an empty spell gem. The RoF2 PlayerProfile writes `0xFFFF_FFFF` into unused
/// `mem_spells[]` slots (see `apply_player_profile`) and OP_MemorizeSpell `scribing=2` (un-memorize)
/// writes the same value; a freshly-constructed `GameState` starts at 0 before the first profile
/// arrives. Both mean "nothing memorized here" — see [`gem_is_empty`]. (eqoxide#348)
pub const EMPTY_GEM: u32 = 0xFFFF_FFFF;

/// True when spell-gem slot content `spell_id` holds no spell. Casting such a gem is a no-op on the
/// server (it never answers), so every caller must refuse it *loudly* rather than queue it.
pub fn gem_is_empty(spell_id: u32) -> bool {
    spell_id == 0 || spell_id == EMPTY_GEM
}

/// How the player's most recent spell cast ENDED (eqoxide#348). Published on
/// `/v1/observe/debug.last_cast` and, as it happens, on the `/v1/events/combat` feed — so an agent
/// can tell *casting* / *landed* / *fizzled* / *interrupted* / *never started* apart instead of
/// scraping free text out of the message log.
#[derive(Debug, Clone, PartialEq)]
pub struct CastOutcome {
    /// The spell that ended, or 0 when the server never told us which (see `GameState::ended_cast_spell`).
    pub spell_id: u32,
    /// `cast_completed` | `cast_interrupted` | `cast_fizzled` | `cast_failed` |
    /// `cast_ended_unexplained` — the same string used as the event `kind`, so the poll and the
    /// push agree. The last one is the client's INFERENCE that a cast ended (the server sent its
    /// cast-end signal and never said why); every other kind is a verdict the server actually gave.
    pub kind: &'static str,
    /// The human-readable line (also written to the message log).
    pub text: String,
    pub at: std::time::Instant,
}

/// The most recent consider result for AN ARBITRARY spawn (#336) — spawn-scoped, independent of the
/// current target. See `GameState::last_consider` for why this exists alongside the older
/// target-scoped `target_con*` fields.
#[derive(Debug, Clone, PartialEq)]
pub struct LastConsider {
    /// The spawn that was considered (the reply's `targetid`, NOT necessarily `target_id`).
    pub spawn_id: u32,
    /// Display name at the time of the consider (best-effort; "Your target" if the spawn had
    /// already left `entities` by the time the reply arrived).
    pub name: String,
    /// Difficulty tier derived from the reply's ConsiderColor `level` field — see `con_level_name`.
    /// gray (trivial/no exp) | green | light_blue | blue | white (even) | yellow | red (dangerous).
    pub con_name: String,
    /// Attitude enum derived from the reply's `faction` field — see `attitude_name`. ally … scowls.
    pub attitude: String,
    /// The spawn's actual character level, if it was in `entities` at consider time. `None` is an
    /// honest "unknown" (e.g. a corpse, or a spawn that despawned between the request and the
    /// reply) — never a fabricated number.
    pub level: Option<u32>,
    pub at: std::time::Instant,
}

/// How long [`GameState::resolve_pending_cast_end`] waits for a packet that EXPLAINS a cast the
/// server has already ended, before reporting the end as unexplained.
///
/// ## This encodes a TIMING ASSUMPTION — state it, don't hide it (see eqoxide#356)
/// The assumption: the explaining packet is queued in the SAME server tick as the OP_ManaChange
/// that ends the cast — `SendSpellBarEnable` then `MemorizeSpell` are back-to-back in
/// `Mob::SpellFinished` (zone/spells.cpp:1817,1824), and `InterruptSpell` likewise emits
/// OP_InterruptCast immediately before its OP_ManaChange (:1306-1314). So this window only has to
/// outlast network jitter, not a game tick, and 400ms is generous for a LAN/loopback server.
///
/// If a loaded server ever split those across ticks, the outcome would degrade to
/// `cast_ended_unexplained` instead of the true reason. That is the SAFE direction — an honest
/// "I don't know why it ended" rather than a confident wrong answer — but it is a real failure mode
/// and a reader should not have to infer it from the constant. Widen this before concluding the
/// client is mis-reporting outcomes on a busy server.
pub const CAST_END_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// How recently OP_ManaChange must have named a spell for that name to be trusted on a failure that
/// carries no spell id of its own. See `GameState::ended_cast_spell`.
pub const CAST_HINT_FRESH: std::time::Duration = std::time::Duration::from_millis(1000);

// ── Cast-outcome string ids (EQEmu zone/string_ids.h) ─────────────────────────────────────────
// The server reports a cast that never started, or that ended badly, as an eqstr id: either inside
// OP_InterruptCast (`InterruptCast_Struct.messageid`, common/eq_packet_structs.h:446) or as a bare
// OP_SimpleMessage (`Client::MessageString`, zone/client.cpp:3803-3823). These are the ids that
// mean "your cast did not happen / did not finish".
/// 173 — "Your spell fizzles!" (zone/string_ids.h:69; raised by the CheckFizzle path, zone/spells.cpp:318-345).
pub const SPELL_FIZZLE: u32 = 173;
/// 180 — "You miss a note, bringing your song to a close!" (the bard fizzle, zone/string_ids.h:71).
pub const MISS_NOTE: u32 = 180;
/// 439 — "Your spell is interrupted." (zone/string_ids.h:177; the default `InterruptSpell` message).
pub const INTERRUPT_SPELL: u32 = 439;
/// Cast-start refusals: the server never begins the cast and only says so as an OP_SimpleMessage.
///   197 "Your spell is too powerful for your intended target."  (zone/spells.cpp:3487)
///   199 "Insufficient Mana to cast this spell!"                 (zone/spells.cpp:490)
///   214 "You must first select a target for this spell!"        (zone/spells.cpp:494 area)
///   236 "Spell recast time not yet met."                        (zone/spells.cpp:1421,
///                                                                zone/client_packet.cpp:9685,9689)
///
/// Every id here has a REAL sender in the server. Ids with no sender were removed: 106
/// ("This spell does not work here.") and 237 ("Spell recovery time not yet met.") appear in
/// zone/string_ids.h but nothing in `zone/*.cpp` ever sends them, so they were dead weight — and
/// each dead entry is a latent unbalanced arm of `suppress_cast_end`. Do not add an id here without
/// checking it has a sender. (eqoxide#348 review)
pub const CAST_FAILED_STRING_IDS: [u32; 4] = [197, 199, 214, 236];

/// One async game event the agent should know about as soon as it happens — surfaced via the
/// `/v1/events/*` feed. `category` is the top-level bucket the events API filters on
/// ("chat" | "combat" | "navigate" | "system"); `kind` is the sub-type within it (e.g. chat →
/// tell/ooc/shout/group/gmsay, navigate → zone, combat → slain/attacked). `directed` = addressed
/// specifically to us (a /tell to our name, a GM message, or something happening to *us*). `id` is
/// monotonic (1-based) per session so an agent can poll `?since=<id>` without missing or re-seeing
/// events. NPC dialogue (say channel) is NOT recorded here — it stays in `messages`.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct ChatLogEvent {
    pub id:       u64,
    pub category: String,  // "chat" | "combat" | "navigate" | "system"
    pub kind:     String,  // sub-type, e.g. "tell"/"ooc"/"zone"/"slain"/"attacked"
    pub from:     String,
    pub directed: bool,
    pub text:     String,
}

/// One member of the player's current group (from OP_GroupUpdateB/OP_GroupUpdate/
/// OP_GroupLeaderChange). `tank`/`assist`/`puller` are read-only role badges the server pushes —
/// eqoxide does not expose a way to set them (v1 scope).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupMember {
    pub name: String,
    pub level: u32,
    pub is_leader: bool,
    pub is_merc: bool,
    pub tank: bool,
    pub assist: bool,
    pub puller: bool,
    pub offline: bool,
}

/// One clickable NPC-dialogue choice parsed from a saylink embedded in an NPC message.
///
/// EQ NPCs offer interactive choices as "saylinks" — links woven into their dialogue text (the
/// server auto-injects one for any `[bracketed]` phrase). Clicking a saylink does NOT send its
/// text; it sends `OP_ItemLinkClick` carrying the link's ids, and the server resolves the phrase
/// from its `saylink` table and processes it as if the player said it to the NPC. So a choice
/// carries the raw link ids needed to rebuild that click packet, plus the display `text`.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct DialogueChoice {
    /// Human-readable label shown between the link delimiters (what a player reads/clicks).
    pub text:      String,
    pub item_id:   u32,        // always SAYLINK_ITEM_ID (0xFFFFF) for a saylink
    pub augments:  [u32; 6],   // augments[0]=sayid (non-silent), augments[1]=sayid (silent)
    pub link_hash: u32,
    pub icon:      u32,        // ornament_icon from the link body
}

/// One item/say link parsed out of chat or NPC text (eqoxide#256).
///
/// `parse_say_links` already hides the raw 56-hex-char link body from every message render path,
/// but until this struct existed the underlying `item_id` was thrown away for anything that
/// WASN'T a dialogue saylink — an agent could read "[Fine Steel Rapier]" but had no id to resolve
/// it against an item lookup. `ItemLink` is the honest middle ground: clean display `text` (what a
/// player reads) PLUS the wire `item_id` (what a lookup needs), attached to the message that
/// contained it.
///
/// `is_saylink` is `true` when `item_id == SAYLINK_ITEM_ID` (0xFFFFF) — a clickable dialogue
/// *phrase*, not a real item (see [`DialogueChoice`], which already exposes the click-to-say
/// mechanism for these). Confirmed against EQEmu `common/say_link.cpp`/`common/features.h` (via
/// eq-client-expert): for a saylink, the link body's augment fields are NOT item augments — they're
/// repurposed as a saylink-table lookup key — so this struct deliberately does not surface them as
/// if they were. When `is_saylink` is `false`, `item_id` is a genuine item-table id.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct ItemLink {
    /// Human-readable label shown between the link delimiters (what a player reads).
    pub text:       String,
    pub item_id:    u32,
    pub is_saylink: bool,
}

/// One guild member from the guild roster (OP_GuildMemberList). Surfaced via GET /v1/guild/roster
/// so agents can see who is in the guild and who is online, the way /v1/group/roster works for a
/// group. (#295)
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct GuildMember {
    pub name:   String,
    /// Rank within the guild: 0 member, 1 officer, 2 leader (RoF2 guildrank).
    pub rank:   u32,
    pub level:  u32,
    /// EQ class id (0 if unknown from the roster record).
    pub class:  u32,
    /// Zone id where the member was last seen (0 = offline). Exposed numerically at the API layer.
    pub zone_id: u32,
    /// True if the member is currently online. Per the RoF2 roster there is no separate flag —
    /// online is derived as `zone_id != 0`.
    pub online: bool,
    /// The member's guild public note (may be empty).
    pub public_note: String,
}

/// One player row from an `OP_WhoAllResponse` roster (`/who all`), so agents can enumerate who is
/// online server-wide (name, level, class, race, zone, guild) before coordinating. (#300)
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct WhoEntry {
    pub name:  String,
    /// EQ level (0 when the player is anonymous — the server zeroes stats for `/anon`).
    pub level: u32,
    /// EQ class id (0 when anonymous). Rendered to a name at the API layer via `class_name`.
    pub class: u32,
    /// EQ race id (0 when anonymous). Rendered to a race code at the API layer.
    pub race:  u32,
    /// Zone id the player is in (0 when anonymous). Exposed numerically at the API layer.
    pub zone_id: u32,
    /// Guild name, empty if none.
    pub guild: String,
    /// True when the player is `/anon` or `/roleplay` — the server suppressed class/level/race/zone.
    pub anon:  bool,
}

/// All state the renderer needs for one frame.
///
/// `PartialEq` is load-bearing: `eq_net::gameplay::publish_snapshot` compares the freshly-mutated
/// `GameState` against the last-published snapshot and only stores a new `Arc` when it actually
/// changed. That makes the published Arc's pointer identity a complete "did anything happen"
/// signal — the render loop's `poll_external` (app.rs) wakes on ANY network-thread mutation
/// (inbound packet OR a client-initiated HTTP request handled by `ActionLoop::tick`), and a
/// genuinely idle world lets the event loop sleep instead of spinning.
#[derive(Debug, Default, Clone, PartialEq)]
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
    /// Player hair color (PlayerProfile `haircolor`, offset 00888). Runtime-tints hair shells only.
    /// (Player hair is not helm-hidden — the player's `showhelm` flag isn't tracked; NPCs are.)
    pub player_haircolor: u8,
    pub player_action: String,
    /// Player's guild id (from the PlayerProfile / spawn `guildID`). `0` = not in a guild (EQEmu's
    /// GUILD_NONE). Resolved to a name via `guild_names` (OP_GuildsList). Exposed at
    /// /v1/observe/debug and used to route/label guild chat. (#295)
    pub player_guild_id: u32,
    /// Player's rank within the guild (guildrank): 0 member, 1 officer, 2 leader (RoF2). (#295)
    pub player_guild_rank: u32,
    /// guild id → guild name, built from OP_GuildsList (the server's guild-name table). Used to
    /// resolve `player_guild_id` and each roster member's guild to a display name. (#295)
    pub guild_names: std::collections::HashMap<u32, String>,
    /// The player's guild roster (from OP_GuildMemberList), for GET /v1/guild/roster. (#295)
    pub guild_members: Vec<GuildMember>,
    /// Latest `/who all` roster (from OP_WhoAllResponse), for GET /v1/observe/who. (#300)
    pub who_roster: Vec<WhoEntry>,
    /// A pending incoming guild invite: (inviter name, guild_id, offered rank). Set when the server
    /// forwards an OP_GuildInvite to us; consumed by POST /v1/guild/accept. (#295)
    pub pending_guild_invite: Option<(String, u32, u32)>,
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
    /// True once `coin` has been seeded from at least one real OP_PlayerProfile coin block (#361).
    /// Gates `reconcile_coin`'s desync report: comparing a genuine starting balance against the
    /// arbitrary all-zero startup default on first login must never be misreported as a "desync".
    pub coin_confirmed: bool,
    /// Count of merchant buys SENT since the last authoritative OP_PlayerProfile reconciliation
    /// (#361). Incremented by `begin_shop_buy` the instant a buy goes out; reset to zero ONLY by
    /// `reconcile_coin` — the sole path that compares `coin` against the server's real balance.
    /// Any nonzero value means at least one buy's outcome is unaccounted-for against server truth:
    /// two merchant-buy refusal paths — inventory-full and a LORE conflict (EQEmu
    /// zone/client_packet.cpp ~14198-14303) — send NO echo of any kind, and inventory-full takes
    /// the coin anyway (the server's own source comments admit the missing refund), so a
    /// silently-refused buy leaves `coin` diverged with no per-buy signal.
    ///
    /// Deliberately a counter, not a bool, and `coin_verified` is a COMPUTED read of it (never an
    /// independently-settable field): a later CONFIRMED buy must NOT be able to clear the standing
    /// uncertainty. `spend_coin` succeeding against an already-stale balance (stale from an EARLIER
    /// silent refusal) would otherwise re-flip a bool back to "verified" while still off by that
    /// refusal — a compounding lie a single per-transaction confirmation cannot be allowed to tell
    /// (reviewer-proven, #361 review, agent-honesty). Only real server truth resolves it.
    pub unverified_buys: u32,
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
    pub safe_x: f32,
    pub safe_y: f32,
    pub safe_z: f32,
    /// Zone "underworld" floor from OP_NewZone (rof2_structs.h @608): the server treats any position
    /// at or below this Z as fallen-through-the-world and does a ZoneToBindPoint recovery. `None`
    /// until OP_NewZone is parsed. The movement controller clamps against it so a collision gap
    /// can't drop us below it and trip the server's below-world drop → CLE linkdead (#150).
    pub zone_underworld: Option<f32>,
    /// Zone distance fog, parsed from OP_NewZone slot 0 (eqoxide#517). `None` until OP_NewZone has
    /// been applied, OR when the zone sends a degenerate/disabled fog range
    /// (`fog_maxclip <= fog_minclip`) — matching the native client's hard FOGENABLE-off behavior
    /// (see `docs/eq-technical-knowledgebase/zone-distance-fog.md`). RoF2's `NewZone_Struct` carries
    /// 4 fog "slots"; only slot 0 (the DB's un-suffixed fog_* columns) is populated by ordinary
    /// zone content, so we only read that one (see the KB doc's "Semantics of the 4 slots" note).
    pub zone_fog: Option<ZoneFog>,
    /// True once OP_NewZone has been applied for the current zone-server session. A RoF2 zone-in
    /// delivers OP_NewZone TWICE: the server sends it unsolicited while handling OP_ZoneEntry and
    /// again in reply to our OP_ReqNewZone (EQEmu `Handle_Connect_OP_ReqNewZone`). The second copy
    /// lands after OP_ReqClientSpawn — i.e. while the spawn/door stream we just asked for is
    /// arriving — so re-running apply_new_zone's entity/door purge would silently wipe it (#322).
    /// `begin_zone_in` re-arms this per zone-server session, so a real zone change still purges.
    pub new_zone_applied: bool,

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
    /// #292: structured con of the current target, from the OP_Consider reply — a difficulty tier
    /// (gray/green/light_blue/blue/white/yellow/red) and a compact attitude enum (ally … scowls),
    /// exposed on /observe/debug so agents can read "how tough" without scraping chat.
    pub target_con_name: Option<String>,
    pub target_attitude: Option<String>,
    /// #336: the result of the MOST RECENT consider of ANY spawn — target or not. Unlike
    /// `target_con*` above (gated on the reply being about the CURRENT target, #330), this is set
    /// unconditionally by every `apply_consider` and is never touched by `set_target`/`clear_target`
    /// — it is spawn-scoped, not target-scoped. This is what closes the standalone-consider gap:
    /// `POST /v1/combat/consider {"id":N}` on a spawn that is deliberately NOT your target used to
    /// compute a difficulty tier and then discard it, leaving no way to learn it without first
    /// targeting the spawn (defeating the whole point of the standalone endpoint).
    pub last_consider: Option<LastConsider>,

    // Zone exit points (populated by OP_SEND_ZONE_POINTS on zone entry)
    pub zone_points: Vec<ZonePoint>,

    // Message log (ring buffer)
    pub messages: VecDeque<LogEntry>,

    /// Text of the most recently read book/note (OP_ReadBook reply), newline-decoded. `None` until a
    /// book has been read this session. Surfaced via GET /v1/observe/item_text. (#288)
    pub last_book_text: Option<String>,

    // Clickable NPC-dialogue choices from the most recent NPC message that carried saylinks
    // (e.g. a Soulbinder's "Do you wish to [bind your soul]?"). Replaced whenever a new NPC
    // message arrives with >=1 saylink; consumed by GET /v1/observe/dialogue, the click API, and
    // the GUI's clickable message HUD.
    pub dialogue_choices: Vec<DialogueChoice>,

    // Inter-agent chat events (tells/ooc/shout/group/gmsay) for the GET /events feed.
    pub chat_events:  VecDeque<ChatLogEvent>,
    pub next_chat_id: u64,

    // UCS (chat server) connection params from OP_SetChatServer; Some once received at zone-in.
    pub ucs: Option<crate::eq_net::ucs::UcsInfo>,

    // Strategy text for HUD
    pub strategy: String,

    /// True from the moment the PLAYER is slain until HP is restored (revive / respawn / heal).
    /// The nav walker checks this to stop driving a corpse toward a stale /goto (eqoxide#61).
    pub player_dead: bool,

    /// Count of server rubber-band corrections (position deltas > 5 units).
    pub server_corrections: u32,

    // Loot state
    /// Corpse spawn_ids queued for auto-looting (populated by OP_BecomeCorpse).
    pub pending_loot: VecDeque<u32>,
    /// True from the moment OP_LootRequest is SENT until the server confirms it closed (via
    /// OP_LootComplete) or refuses it (via OP_MoneyOnCorpse with a non-accept response). Do not
    /// read this alone as "the corpse is open" — see `loot_confirmed` (#346).
    pub loot_session_active: bool,
    /// True only once the server has actually accepted the loot request (OP_MoneyOnCorpse with
    /// response Normal/Normal2/LootAll). Distinguishes "we asked" from "it opened" — a refused
    /// corpse (SomeoneElse/NotAtThisTime/Hostiles/TooFar) never sets this (#346).
    pub loot_confirmed: bool,
    /// Spawn id of the corpse the current loot session is open against, if any. Needed to build a
    /// well-formed OP_EndLootRequest (the server requires the corpse's spawn_id as its payload —
    /// an empty payload is silently dropped, #346) and to name the corpse in refusal messages.
    pub loot_current_corpse: Option<u32>,
    /// Updated each time the server sends a loot-related packet; used to notice item echoes have
    /// gone quiet so it's time to ask the server to close the session (OP_EndLootRequest). This
    /// no longer decides when "Looting complete" is reported — that only ever comes from the
    /// inbound OP_LootComplete handler (#346).
    pub loot_last_activity: Option<std::time::Instant>,
    /// Set when OP_EndLootRequest has been sent and we're waiting for the server's OP_LootComplete
    /// close ack. If this elapses past a timeout with no ack, the session is reported as failed
    /// (distinct from "complete") rather than silently assumed done (#346).
    pub loot_end_requested_at: Option<std::time::Instant>,
    /// When the first corpse was pushed to pending_loot; used to delay LootRequest by
    /// 500 ms so the server has time to register the corpse as lootable.
    pub loot_queued_at: Option<std::time::Instant>,
    /// #414: set when we've given up waiting on a loot-ack for the CURRENT corpse — either
    /// `OP_MoneyOnCorpse` never arrived (`OpenTimedOut`) or `OP_LootComplete` never arrived
    /// after we asked to close (`TimedOut`) — and have sent (or, for the close-side, already
    /// sent) a defensive/idempotent `OP_EndLootRequest` to release the server-side lock
    /// (`Corpse::EndLoot` doesn't check ownership — safe even for a never-confirmed corpse; see
    /// docs/eq-technical-knowledgebase/loot-protocol.md). While this is `Some`, `loot_current_corpse`
    /// and `loot_session_active` are deliberately left untouched so `loot_tick_action` withholds
    /// the NEXT corpse's `OP_LootRequest` until this one's fate is truly settled — narrowing (not
    /// eliminating; neither `OP_MoneyOnCorpse` nor `OP_LootComplete` carries a corpse id at all)
    /// the window in which a late ack for THIS corpse could otherwise land on a different, later
    /// session and be misattributed to it. A reply that arrives while this is `Some` is drained
    /// silently (the definitive failure was already reported when this was set) — see
    /// `apply_loot_complete`'s branch 0 and `apply_money_on_corpse`'s stale-ack gate.
    pub loot_defensive_close_at: Option<std::time::Instant>,

    // Quest log (native EQ Task system) — server-pushed via OP_TaskDescription / OP_TaskActivity.
    /// All task quests keyed by task_id (any status), with their objectives + live progress.
    pub tasks: std::collections::HashMap<u32, ActiveTask>,
    /// Pending offers from an open task-selector window (OP_TaskSelectWindow). Replaced wholesale
    /// on each new window; cleared after an accept/decline is sent.
    pub task_offers: Vec<TaskOffer>,
    /// Completed-task history with titles, from OP_CompletedTasks (server sends the full record,
    /// not bare ids — see `apply_completed_tasks`).
    pub completed_task_history: Vec<CompletedTaskEntry>,

    /// Player inventory + equipment (decoded from OP_CharInventory / OP_ItemPacket).
    pub inventory: Vec<InvItem>,

    /// Set true when the server sends OP_TradeRequestAck — the trade session now exists, so the
    /// nav thread may move the cursor item into the NPC trade slot and accept. Cleared once the
    /// give state machine consumes it (or on timeout). See navigation.rs.
    pub trade_ack_ready: bool,

    // Spellcasting / posture
    /// Memorized spell gem IDs (9 slots); 0xFFFF_FFFF = empty slot.
    pub mem_spells: [u32; 9],
    /// Player skill values by skill id (0..77), from PlayerProfile `skills[]` (eqoxide#99).
    /// 0 = untrained; empty until the first PlayerProfile arrives. Exposed via GET
    /// /v1/observe/skills; the trainer raises these. (Vec, not `[u32; 77]`: arrays > 32 don't
    /// derive Default/Serialize.)
    pub player_skills: Vec<u32>,
    /// Open guildmaster-training window: the trainer NPC's spawn id, set when the server replies to
    /// OP_GMTraining, cleared on close. `None` = no trainer window open (eqoxide#99).
    pub trainer_open: Option<u32>,
    /// Skill CAPS the open trainer offers, by skill id (0..77), from the OP_GMTraining reply's
    /// `skills[]`. `cap == 0` = the class can't train that skill here; trainable = cap > current.
    pub trainer_skills: Vec<u32>,
    /// Active cast in progress (Some) or idle (None).
    pub casting: Option<CastState>,
    /// How the player's most recent cast ended (eqoxide#348). Kept after the cast so a slow poller
    /// of `/v1/observe/debug` still learns the outcome it missed on the event feed.
    pub last_cast: Option<CastOutcome>,
    /// spell_id of the cast the server most recently told us STOPPED, and when it said so — from
    /// OP_ManaChange with `keepcasting == 0`, which both `Mob::StopCasting` (zone/spells.cpp:1369)
    /// and `Mob::SendSpellBarEnable` (zone/spells.cpp:5752) send with `spell_id = the cast that
    /// ended`. It is the ONLY way to name the spell in a *fizzle*: EQEmu decides a fizzle in
    /// `DoCastSpell` (zone/spells.cpp:320) **before** it ever sends OP_BeginCast
    /// (zone/spells.cpp:450), so `casting` is still `None` when the fizzle message arrives.
    ///
    /// Consumed (taken) by [`GameState::finish_cast`] AND time-scoped ([`CAST_HINT_FRESH`]): the
    /// server re-arms this on the SendSpellBarEnable that TRAILS an interrupt/refusal
    /// (zone/spells.cpp:1314) and on the Lua-only `ResetAllCastbarCooldowns` burst
    /// (zone/spells.cpp:7246), so an un-scoped hint would pin a stale, unrelated spell name on the
    /// next failure. (eqoxide#348)
    pub ended_cast_spell: Option<(u32, std::time::Instant)>,
    /// A cast the server has ENDED (OP_ManaChange `keepcasting=0`) but not yet EXPLAINED. Armed
    /// only when a cast was actually in flight; cleared by whichever packet refines it into a real
    /// outcome (memorize=completed / interrupt / message). If nothing refines it within
    /// [`CAST_END_GRACE`], [`GameState::resolve_pending_cast_end`] reports it as an explicit
    /// unexplained end rather than letting `casting` hang forever. (eqoxide#348)
    pub pending_cast_end: Option<std::time::Instant>,
    /// Ignore the next OP_ManaChange(`keepcasting=0`), because we have ALREADY reported the outcome
    /// it belongs to. `Mob::InterruptSpell` sends OP_InterruptCast and THEN `SendSpellBarEnable`
    /// (zone/spells.cpp:1299-1314); a cast-start refusal likewise sends its OP_SimpleMessage and
    /// then `StopCastSpell` → `SendSpellBarEnable`. Without this, that trailing ManaChange would
    /// re-arm `ended_cast_spell` with a spell we just finished reporting, and the next unnamed
    /// failure would inherit it.
    ///
    /// ## Deliberately a bool, and reset on every cast — it is NOT a counter
    /// A counter here would be a landmine. Its correctness would rest on a conservation law that is
    /// FALSE: "every refusal is followed by exactly one OP_ManaChange". `Mob::CastSpell` sets
    /// `send_spellbar_enable = false` for an instant-cast item clicky or an AA
    /// (`(item_slot != -1 && cast_time == 0) || aa_id` — zone/spells.cpp:158-161), so
    /// `StopCastSpell` skips `SendSpellBarEnable` ENTIRELY and no terminal ManaChange is ever sent.
    /// SPELL_TOO_POWERFUL (197) reaches exactly that path, and eqoxide has an item-clicky cast path
    /// (`/v1/combat/cast {"item_slot":N}`).
    ///
    /// An unbalanced increment on a counter would then never be decremented — silently eating the
    /// terminal ManaChange of some LATER cast, so `casting` hangs forever with no outcome event.
    /// Permanent, session-wide, and triggered by something that happened minutes earlier: the exact
    /// bug that gets written off as "the client randomly gets stuck sometimes".
    ///
    /// A bool cannot accumulate, and [`GameState::begin_cast`] / [`GameState::begin_zone_in`] clear
    /// it, so a missing terminal can affect at most the cast it belongs to. (eqoxide#348 review)
    pub suppress_cast_end: bool,
    /// True when the player is sitting.
    pub sitting: bool,
    /// When the player's own death was first observed (OP_Death for our spawn), or None
    /// while alive. Used to (a) dedupe the duplicate OP_Death the server sometimes sends
    /// and (b) drive the respawn safety-net that re-requests a bind respawn when the
    /// server never opens (or never honors) the respawn window. Cleared once HP is
    /// restored. Transient recovery bookkeeping. (eqoxide#50)
    pub player_dead_since: Option<std::time::Instant>,
    /// Name of whatever last killed the player (from OP_Death's killer_id), and when the death
    /// happened. Unlike `player_dead_since` these PERSIST past the respawn so `/v1/observe/debug`
    /// can report a recent death (dead / killed_by / died_ago_secs) even after reviving. (#284)
    pub killed_by: String,
    pub died_at: Option<std::time::Instant>,
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

    /// Current group roster (empty = not grouped). Full-replaced by OP_GroupUpdateB, incrementally
    /// updated by OP_GroupUpdate/OP_GroupDisbandOther/OP_GroupLeaderChange.
    pub group_members: Vec<GroupMember>,
    /// Current group leader's name ("" if unknown/not grouped).
    pub group_leader: String,
    /// Inviter's name while an incoming invite awaits accept/decline via POST
    /// /v1/group/accept|decline. None when there's no open invite.
    pub pending_invite: Option<String>,
}

impl GameState {
    pub fn new() -> Self {
        GameState {
            messages: VecDeque::with_capacity(50),
            ..Default::default()
        }
    }

    /// Start a zone-server session (login zone handoff, or an in-game zone change): purge the
    /// previous zone's spawns and doors and re-arm the once-per-zone-in OP_NewZone apply. Called at
    /// the top of each zone-entry handshake, before OP_ReqClientSpawn asks for the spawn stream, so
    /// the clear can never race the stream it precedes. (#322)
    pub fn begin_zone_in(&mut self) {
        self.entities.clear();
        self.doors.clear();
        // The target belongs to the zone we just left: its spawn id is meaningless in the new zone
        // and #270 already purges `entities`, so target_id would point at a gone spawn while
        // target_name/target_hp_pct fall back to the stale cached snapshot — /observe/debug then
        // reports a full-HP target from the OLD zone (a confident falsehood an agent may attack /
        // consider). Clear the whole target (id + name + hp + con) here, not just the entity map (#408).
        self.clear_target();
        self.new_zone_applied = false;
        // A cast cannot survive a zone change: the spawn ids, the cast bar and every packet that
        // would have explained the cast belong to the zone we just left. Carrying `casting` across
        // would report a cast in flight that can never end, and carrying `suppress_cast_end` would
        // eat the terminal of the first cast in the NEW zone. (eqoxide#348 review)
        self.reset_cast_tracking();
        self.casting = None;
    }

    /// Drop all in-flight cast bookkeeping (but NOT `last_cast`, which is a true record of
    /// something that already happened). Shared by [`GameState::begin_cast`] and
    /// [`GameState::begin_zone_in`]. (eqoxide#348 review)
    fn reset_cast_tracking(&mut self) {
        self.pending_cast_end = None;
        self.ended_cast_spell = None;
        self.suppress_cast_end = false;
    }

    /// Is the player slain? Detected the SAME way the render/anim path picks the dead pose
    /// (`cur_hp <= 0` with a known `max_hp`) OR via the OP_Death `player_dead` flag. Using cur_hp —
    /// not just `player_dead` — catches an HP-to-0 update that lands before OP_Death arrives, which
    /// is the window in which a corpse was seen still walking (#238). Shared by the nav walker
    /// (`nav::walker::Walker::nav_halt_if_dead`) and the auto zone-cross guard
    /// (`eq_net::action_loop::ActionLoop::drain_zone_cross`) — moved here (out of `ActionLoop`) so
    /// both can call it without one depending on the other's private items (M1 walker extraction).
    pub fn is_player_dead(&self) -> bool {
        self.player_dead || (self.cur_hp <= 0 && self.max_hp > 0)
    }

    pub fn log_msg(&mut self, kind: &str, text: &str) {
        self.log_msg_with_item_links(kind, text, Vec::new());
    }

    /// Same as [`GameState::log_msg`], but also attaches any [`ItemLink`]s found in `text` (parsed
    /// by `parse_say_links` in `eq_net::packet_handler`) so a caller reading the message log gets a
    /// resolvable item reference alongside the clean display text (eqoxide#256).
    pub fn log_msg_with_item_links(&mut self, kind: &str, text: &str, item_links: Vec<ItemLink>) {
        // 400 entries so the chat window has real scrollback (was 50).
        if self.messages.len() >= 400 {
            self.messages.pop_front();
        }
        self.messages.push_back(LogEntry {
            kind: kind.to_string(),
            text: text.to_string(),
            timestamp: std::time::Instant::now(),
            item_links,
        });
    }

    /// Resolve a group member's real level. The RoF2 OP_GroupUpdateB packet carries a hardcoded
    /// placeholder level (EQEmu's encoder writes 0x46=70 for the leader and 0x41=65 for every other
    /// member — not the real value, eqoxide#104), so take the level from our own profile (self) or
    /// the member's spawn in the entity list. Returns 0 (unknown) when the member isn't in the zone.
    pub fn group_member_level(&self, name: &str) -> u32 {
        if !self.player_name.is_empty() && name == self.player_name {
            self.player_level
        } else {
            self.entities.values().find(|e| e.name == name).map(|e| e.level).unwrap_or(0)
        }
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

    /// The player's own cast bar has started (their OP_BeginCast came back). Publishes a
    /// `combat`/`cast_begin` event so an agent long-polling `/v1/events/*` learns the server
    /// actually accepted the cast — the previous code set `casting` and told nobody. (eqoxide#348)
    pub fn begin_cast(&mut self, spell_id: u32, cast_ms: u32) {
        // A new cast starts from a CLEAN slate. Every one of these is bookkeeping for the PREVIOUS
        // cast, and any of it that survives is a booby trap for this one — most dangerously
        // `suppress_cast_end`, which the server can leave armed with no terminal to balance it (see
        // its doc comment). Resetting here bounds that damage to the cast it came from.
        self.reset_cast_tracking();
        self.casting = Some(CastState { spell_id, started: std::time::Instant::now(), cast_ms });
        self.last_cast = None; // a new cast supersedes the previous outcome
        let text = format!("You begin casting {}.", crate::spells::name_of(spell_id));
        self.log_msg("spell", &text);
        self.push_event("combat", "cast_begin", "", true, &text);
    }

    /// Terminal outcome for the player's cast: clear the cast bar, remember it for
    /// `/v1/observe/debug`, log it, and push it onto the `/v1/events/combat` feed. `kind` is one of
    /// `cast_completed` / `cast_interrupted` / `cast_fizzled` / `cast_failed`.
    ///
    /// `spell_id`: pass the id if the packet carried one (OP_MemorizeSpell does); otherwise pass 0
    /// and we fall back to the in-flight cast, then to the id OP_ManaChange reported as ended (the
    /// fizzle case, where no OP_BeginCast was ever sent). 0 = "the server never told us which
    /// spell" — an honest unknown, not a guess. (eqoxide#348)
    pub fn finish_cast(&mut self, spell_id: u32, kind: &'static str, text: &str) {
        let spell_id = if spell_id != 0 {
            spell_id
        } else {
            self.casting.as_ref().map(|c| c.spell_id)
                // Only a FRESH hint may name the spell. A stale one is worse than no name at all:
                // it is a plausible-looking lie. 0 = "the server never told us which spell".
                .or_else(|| self.ended_cast_spell
                    .filter(|(_, at)| at.elapsed() < CAST_HINT_FRESH)
                    .map(|(id, _)| id))
                .unwrap_or(0)
        };
        self.casting = None;
        self.ended_cast_spell = None; // consumed — never reuse it for a later cast
        self.pending_cast_end = None; // a real outcome supersedes the unexplained-end timeout
        self.last_cast = Some(CastOutcome {
            spell_id,
            kind,
            text: text.to_string(),
            at: std::time::Instant::now(),
        });
        self.log_msg("spell", text);
        self.push_event("combat", kind, "", true, text);
    }

    /// The server ENDED the player's cast (OP_ManaChange `keepcasting=0` — its universal cast-end
    /// signal) without yet saying *why*. Clear the cast bar immediately (the cast is genuinely
    /// over) and start the grace window in which a following packet may still explain it.
    ///
    /// Clearing here is what makes `casting` un-stickable. `Mob::SpellFinished` can return false —
    /// a beneficial buff that won't stack is the common case (zone/spells.cpp:2590 → :1744-1751) —
    /// and then `CastedSpellFinished` calls `StopCasting()`, which sends this ManaChange and
    /// **nothing else**: no memorize, no interrupt, no message. Without a terminal here, re-buffing
    /// an already-buffed target left `casting` set forever. (eqoxide#348)
    pub fn end_cast_unexplained(&mut self) {
        if self.casting.is_none() { return; } // no cast in flight → nothing to end (see caller)
        self.casting = None;
        self.pending_cast_end = Some(std::time::Instant::now());
    }

    /// Called every gameplay tick. If the server ended a cast and never explained it within
    /// [`CAST_END_GRACE`], say so — but say it as what it IS.
    ///
    /// This is deliberately **not** `cast_failed`. `cast_failed` means "the server told us the cast
    /// failed" — that is knowledge, carried by a real server string. An unexplained end means "the
    /// server told us nothing; we inferred the cast ended" — that is an inference. Collapsing the
    /// two would hand the agent a verdict the client does not actually have, and phrasing it in
    /// server voice ("Your spell did not take hold") would make our guess indistinguishable from
    /// something the server said. An agent must be able to branch on the difference.
    ///
    /// The same rule governs `spell_id`: an unnamed spell reports 0, because a plausibly-wrong name
    /// is a lie while an honest "unknown" is not. (eqoxide#348)
    pub fn resolve_pending_cast_end(&mut self) {
        let Some(at) = self.pending_cast_end else { return };
        if at.elapsed() < CAST_END_GRACE { return; }
        self.pending_cast_end = None;
        let spell_id = self.ended_cast_spell
            .filter(|(_, t)| t.elapsed() < CAST_HINT_FRESH)
            .map(|(id, _)| id)
            .unwrap_or(0);
        // Client's-own-voice, explicitly an observation — never a fabricated server line.
        let text = format!(
            "The cast of {} ended with no outcome reported by the server \
             (observed by the client; the server said nothing).",
            crate::spells::name_of(spell_id),
        );
        self.finish_cast(spell_id, "cast_ended_unexplained", &text);
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

    /// Call immediately before sending an OP_ShopRequest to open merchant `merchant_id` — the first
    /// packet of every buy, sell, and explicit `/v1/merchant/open` (#360). `Handle_OP_ShopRequest`
    /// (EQEmu zone/client_packet.cpp) sends NO echo at all on a failed request — a non-merchant
    /// target (:14605-14607) or an out-of-range one (:14610-14612) — so without this,
    /// `merchant_open` would keep reporting the PREVIOUS merchant's id forever after such a
    /// request. Clearing it optimistically at send time makes the stale-lie unrepresentable: only
    /// the server's OP_ShopRequest echo (`apply_shop_request`) may set it again, so an unanswered
    /// request now reads as "not open" instead of "still open on the last one".
    ///
    /// Clears ONLY when this is genuinely a (re)open of a DIFFERENT merchant, or a first open. The
    /// routine pre-buy/pre-sell OP_ShopRequest resend against the merchant that's ALREADY open must
    /// NOT flicker `merchant_open` to None for a round-trip — `sync_merchant` mirrors it into the
    /// HTTP snapshot every tick and the HUD gates the window on `is_some()`, so a blind clear-then-
    /// reconfirm made every buy/sell briefly report the open merchant as closed, a new false
    /// negative (#361 review — FIX 2). A stale id from an earlier failed request differs from this
    /// target and is still cleared.
    pub fn begin_shop_open_for(&mut self, merchant_id: u32) {
        if self.merchant_open != Some(merchant_id) {
            self.merchant_open = None;
            self.merchant_items.clear();
        }
    }

    /// Call immediately before sending the OP_ShopPlayerBuy packet itself (#361). Records that a
    /// buy is now outstanding and unaccounted-for against server truth (`unverified_buys += 1`), so
    /// `coin_verified()` reads false until the next `reconcile_coin`. A silent buy refusal
    /// (inventory-full or LORE conflict, EQEmu zone/client_packet.cpp ~14198-14303) sends no echo
    /// of any kind, so the client cannot tell success from failure per-buy; only an OP_PlayerProfile
    /// clears the uncertainty. Saturating so a marathon shopping run can never wrap the counter.
    pub fn begin_shop_buy(&mut self) {
        self.unverified_buys = self.unverified_buys.saturating_add(1);
    }

    /// True only when `coin` is known to match the server's real balance: a genuine reading has
    /// landed (`coin_confirmed`) AND no merchant buy has been sent since the last authoritative
    /// reconciliation (`unverified_buys == 0`). Computed — never an independently-settable field —
    /// so no single per-transaction confirmation can assert trust the client has not actually
    /// verified against server truth (#361, agent-honesty).
    pub fn coin_verified(&self) -> bool {
        self.coin_confirmed && self.unverified_buys == 0
    }

    /// Reconcile the local coin snapshot against the server's authoritative figure, carried by
    /// every OP_PlayerProfile (#361). Two merchant-buy refusal paths — inventory-full and a LORE
    /// conflict — send no echo of any kind, and for inventory-full the server takes the coin
    /// anyway (EQEmu's own source comments at zone/client_packet.cpp:14258-14259 and :14286 admit
    /// the bug), so a silently-refused buy can leave `coin` diverged from the real balance with
    /// nothing else to correct it.
    ///
    /// Corrects `coin` unconditionally, marks it confirmed, and clears `unverified_buys` back to
    /// zero — the figure is now fresh from the source of truth, so `coin_verified()` reads true and
    /// every outstanding buy's uncertainty is resolved at once. This is the ONLY path that clears
    /// that uncertainty: a per-buy echo cannot, because it confirms a relative delta, not that the
    /// absolute balance escaped an earlier silent refusal (#361 review). Returns the stale prior
    /// balance ONLY when it disagreed with the server's figure AND a real prior reading already
    /// existed; comparing a genuine starting balance against the arbitrary all-zero startup default
    /// on first login must never be misreported as a desync.
    pub fn reconcile_coin(&mut self, server_coin: [u32; 4]) -> Option<[u32; 4]> {
        let prior = self.coin;
        let desynced = self.coin_confirmed && prior != server_coin;
        self.coin = server_coin;
        self.coin_confirmed = true;
        self.unverified_buys = 0;
        if desynced { Some(prior) } else { None }
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
            self.clear_target(); // #331: also drops the now-stale name/hp/con, not just the id
        }
        if self.pet_id == Some(spawn_id) {
            self.pet_id = None; // pet died / despawned
        }
    }

    /// Select a new target and clear every piece of PREVIOUS-target derived state in the
    /// same call, so nothing can leak across a re-target (eqoxide#323). Before this existed,
    /// every target-select call site set `target_id` (and sometimes `target_name`) inline and
    /// left `target_con`/`target_con_name`/`target_attitude` untouched — those three only ever
    /// get written by a fresh OP_Consider reply (`apply_consider`), so a trivial mob targeted
    /// right after a dangerous one rendered with the OLD red con until the next consider
    /// reply landed (or forever, for a spawn — e.g. a corpse — the server never considers).
    /// `target_name`/`target_hp_pct` had the same problem for any id not present in
    /// `gs.entities` (a corpse, an out-of-range spawn, a stale/bogus id): the previous
    /// target's name/HP just stayed put instead of clearing.
    ///
    /// `target_name`/`target_hp_pct` are seeded from `entities[id]`, except for the F1
    /// self-target case (`id == player_id`): the player is never present in `entities`
    /// (`register_spawn` special-cases and skips the self-spawn), so self-target must resolve
    /// name/HP from the player's own fields instead — mirrors the entity-name idiom used for
    /// combat-log lines elsewhere (packet_handler.rs) and the self-target branch already
    /// covered by `update_hp`'s live-sync (eqoxide#9, #291). Any OTHER unknown id clears
    /// `target_name`/`target_hp_pct` to `None` rather than leaving the previous target's
    /// values in place.
    pub fn set_target(&mut self, id: u32) {
        self.target_id = Some(id);
        self.target_con = None;
        self.target_con_name = None;
        self.target_attitude = None;
        if id == self.player_id {
            self.target_name = Some(self.player_name.clone());
            self.target_hp_pct = Some(self.hp_pct);
        } else if let Some(e) = self.entities.get(&id) {
            self.target_name = Some(e.name.clone());
            self.target_hp_pct = Some(e.hp_pct);
        } else {
            self.target_name = None;
            self.target_hp_pct = None;
        }
    }

    /// Counterpart to [`GameState::set_target`] for "no target" (eqoxide#331): nulls every
    /// target-derived field, not just `target_id`. Before this existed, `remove_entity` cleared
    /// only `target_id` on a kill, leaving `target_name`/`target_hp_pct` (and, had anything
    /// otherwise raced it, `target_con`/`target_con_name`/`target_attitude`) pointing at the
    /// now-dead mob. The HUD hid the leak (it requires both id and name to be `Some`), but the
    /// `/v1/observe/debug` HTTP snapshot doesn't, so it reported a dead target's name/HP forever
    /// after every kill.
    pub fn clear_target(&mut self) {
        self.target_id = None;
        self.target_name = None;
        self.target_hp_pct = None;
        self.target_con = None;
        self.target_con_name = None;
        self.target_attitude = None;
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
            // Alive again → clear the death/respawn bookkeeping. (eqoxide#61, #50)
            if cur_hp > 0 {
                self.player_dead = false;       // revived / healed above 0
                self.player_dead_since = None;  // clear the respawn safety-net timer
            }
        } else if let Some(e) = self.entities.get_mut(&spawn_id) {
            e.cur_hp = cur_hp;
            e.max_hp = max_hp;
            e.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
        }
        // Keep the target HUD's HP gauge live: target_hp_pct is a stored snapshot (seeded
        // when the target is selected — see ActionLoop::tick), not derived fresh from
        // `entities` on every read, so it must be refreshed here whenever the update is for
        // whichever spawn is currently targeted (mob or self via F1). (eqoxide#9, task 6)
        if self.target_id == Some(spawn_id) {
            self.target_hp_pct = Some(self.hp_pct).filter(|_| spawn_id == self.player_id)
                .or_else(|| self.entities.get(&spawn_id).map(|e| e.hp_pct));
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
            // Same live-refresh as update_hp (this path never fires for the player — see guard
            // above — so no self-target branch is needed here).
            if self.target_id == Some(spawn_id) {
                self.target_hp_pct = Some(hp_pct);
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

/// Split NPC dialogue text into runs, flagging `[bracketed]` quest keywords.
/// An unterminated `[` run is treated as plain text. Shared by the dialogue
/// window (clickable keywords) and the HTTP message feed (keyword extraction).
pub fn split_keywords(text: &str) -> Vec<(String, bool)> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        if open > 0 {
            out.push((rest[..open].to_string(), false));
        }
        if let Some(close_rel) = rest[open..].find(']') {
            let close = open + close_rel;
            out.push((rest[open..=close].to_string(), true));
            rest = &rest[close + 1..];
        } else {
            out.push((rest[open..].to_string(), false));
            rest = "";
            break;
        }
    }
    if !rest.is_empty() {
        out.push((rest.to_string(), false));
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{Door, Entity, GameState, MerchantItem};

    /// eqoxide#201: the flat bag-slot mapping must round-trip and match the RoF2 numbering
    /// (GENERAL_BAGS_BEGIN=251, stride 10, parent general slots 23-32).
    #[test]
    fn bag_wire_slot_maps_and_round_trips() {
        use super::{bag_wire_slot, bag_wire_parent};
        // First general bag (slot 23), sub 0 → 251; sub 9 → 260. Second bag (24) sub 0 → 261.
        assert_eq!(bag_wire_slot(23, 0), Some(251));
        assert_eq!(bag_wire_slot(23, 9), Some(260));
        assert_eq!(bag_wire_slot(24, 0), Some(261));
        assert_eq!(bag_wire_slot(32, 9), Some(350)); // last general bag, last sub
        // Out of range → None (not a general container / bad sub-index).
        assert_eq!(bag_wire_slot(22, 0), None); // worn slot, not a bag parent
        assert_eq!(bag_wire_slot(33, 0), None); // cursor bags unsupported for move
        assert_eq!(bag_wire_slot(23, 10), None);
        // Inverse round-trips for every general bag/sub combination.
        for parent in 23..=32 {
            for sub in 0..10u32 {
                let flat = bag_wire_slot(parent, sub).unwrap();
                assert_eq!(bag_wire_parent(flat), Some((parent, sub)));
            }
        }
        // Non-bag flats decode to None.
        assert_eq!(bag_wire_parent(33), None);
        assert_eq!(bag_wire_parent(250), None);
        assert_eq!(bag_wire_parent(351), None);
    }

    pub(crate) fn make_entity(id: u32, name: &str, x: f32, y: f32, z: f32, is_npc: bool) -> Entity {
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
            face: 0, hairstyle: 0, haircolor: 0,
            animation: 0, floating: false,
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
    fn begin_shop_open_clears_a_stale_previous_merchant() {
        // #360: sending a NEW OP_ShopRequest (for merchant B) must not leave `merchant_open`
        // reporting the PREVIOUS merchant (A) if B's request is refused silently (no echo at
        // all — out-of-range/non-merchant, EQEmu client_packet.cpp:14605-14612). Clearing at
        // send time means a request that never gets answered reads as "not open", never "still
        // open on A".
        let mut gs = GameState::new();
        gs.merchant_open = Some(111); // merchant A, from a prior successful open
        gs.merchant_items.push(MerchantItem { merchant_slot: 1, item_id: 1, name: "Rusty Dagger".into(), icon: 0, price: 5, quantity: 1 });

        gs.begin_shop_open_for(222); // about to send OP_ShopRequest for a DIFFERENT merchant B

        assert_eq!(gs.merchant_open, None, "must not still report the previous merchant (A) as open");
        assert!(gs.merchant_items.is_empty(), "stale wares list must not survive either");
    }

    #[test]
    fn begin_shop_open_for_an_already_open_merchant_does_not_flicker_it_closed() {
        // #361 review (FIX 2): the pre-buy/pre-sell OP_ShopRequest resend targets the merchant
        // that's ALREADY open. Clearing merchant_open here would flicker the HUD/`/v1/merchant/list`
        // to "closed" for a round-trip against a merchant that never closed — a new false negative.
        let mut gs = GameState::new();
        gs.merchant_open = Some(111);
        gs.merchant_items.push(MerchantItem { merchant_slot: 1, item_id: 1, name: "Rusty Dagger".into(), icon: 0, price: 5, quantity: 1 });

        gs.begin_shop_open_for(111); // re-open the SAME merchant (routine pre-buy resend)

        assert_eq!(gs.merchant_open, Some(111), "an already-open merchant must not flicker closed");
        assert!(!gs.merchant_items.is_empty(), "its wares list must survive the resend too");
    }

    #[test]
    fn begin_shop_buy_marks_coin_unverified_until_resolved() {
        // #361: the moment a buy is sent, coin becomes provisionally unverified — a silent
        // refusal (inventory-full/LORE) sends no echo at all, so we cannot know yet whether the
        // server's balance still matches ours.
        let mut gs = GameState::new();
        gs.coin_confirmed = true; // a real reading had landed, so coin_verified() was true
        assert!(gs.coin_verified());
        gs.begin_shop_buy();
        assert!(!gs.coin_verified(), "coin must be unverified the instant a buy is in flight");
    }

    #[test]
    fn a_confirmed_buy_cannot_re_verify_coin_left_stale_by_an_earlier_silent_refusal() {
        // #361 review (FIX 1, reviewer-proven PoC): coin_verified must not be a bool any single
        // confirmation can clear. Scenario:
        //   * coin confirmed+verified.
        //   * Buy #1 is a TRUE silent inventory-full refusal — begin_shop_buy runs, then nothing
        //     (no echo). coin stays put locally but server truth has silently dropped.
        //   * Buy #2 is a normal CONFIRMED purchase the still-stale local balance can cover.
        // The confirmed buy #2 must NOT flip coin back to "verified": the earlier silent refusal
        // is still unaccounted-for, so only a real OP_PlayerProfile may restore trust.
        let mut gs = GameState::new();
        gs.coin = [100, 0, 0, 0];
        gs.coin_confirmed = true;
        assert!(gs.coin_verified(), "precondition: a real reading had established trust");

        gs.begin_shop_buy(); // buy #1 sent — silent inventory-full refusal, never echoes
        gs.begin_shop_buy(); // buy #2 sent
        // buy #2's confirmed echo would run spend_coin against the still-stale [100,..]; simulate
        // the balance-covering deduction that the OLD bool would have re-verified on.
        assert!(gs.spend_coin(20));
        assert!(!gs.coin_verified(),
            "a confirmed buy cannot earn back trust while an earlier silent refusal is unresolved");

        // Only the authoritative server profile clears the standing uncertainty.
        gs.reconcile_coin([50, 0, 0, 0]);
        assert!(gs.coin_verified(), "a real OP_PlayerProfile is the sole path back to verified");
    }

    #[test]
    fn reconcile_coin_corrects_a_silent_divergence_and_reports_it() {
        // The inventory-full refusal path takes the player's coin server-side but sends no echo
        // (EQEmu client_packet.cpp: TakeMoneyFromPP @14261-14278 runs before the free-slot check
        // @14282-14303 that can fail) — so the client's balance silently overstates reality until
        // the next OP_PlayerProfile arrives and this reconciles it.
        let mut gs = GameState::new();
        gs.coin = [10, 0, 0, 0];
        gs.coin_confirmed = true; // we already had a real prior reading (not first login)
        gs.begin_shop_buy();      // a buy is in flight, outcome unknown
        assert!(!gs.coin_verified());

        let prior = gs.reconcile_coin([9, 5, 0, 0]); // server says less than we believed

        assert_eq!(prior, Some([10, 0, 0, 0]), "a real mismatch must be reported, not swallowed");
        assert_eq!(gs.coin, [9, 5, 0, 0], "coin must be corrected to the server's authoritative figure");
        assert!(gs.coin_verified(), "the figure is now fresh from the source of truth");
        assert!(gs.coin_confirmed);
    }

    #[test]
    fn reconcile_coin_agrees_reports_nothing() {
        let mut gs = GameState::new();
        gs.coin = [9, 5, 0, 0];
        gs.coin_confirmed = true;
        let prior = gs.reconcile_coin([9, 5, 0, 0]);
        assert_eq!(prior, None, "matching figures are not a desync");
        assert!(gs.coin_verified());
    }

    #[test]
    fn reconcile_coin_first_login_never_misreports_the_zero_default_as_a_desync() {
        // A fresh GameState starts coin=[0,0,0,0], coin_confirmed=false. The FIRST real
        // OP_PlayerProfile a returning (non-broke) character receives will almost always disagree
        // with that arbitrary startup default — this must never be reported as a "desync".
        let mut gs = GameState::new();
        assert!(!gs.coin_confirmed, "precondition: no real coin reading has ever landed yet");
        assert_eq!(gs.coin, [0, 0, 0, 0]);

        let prior = gs.reconcile_coin([12, 3, 4, 5]); // the character's actual starting balance

        assert_eq!(prior, None, "seeding the very first real balance is not a desync");
        assert_eq!(gs.coin, [12, 3, 4, 5]);
        assert!(gs.coin_confirmed);
        assert!(gs.coin_verified());
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
        // Fill to exactly the ring cap (400 — sized for chat scrollback, #162).
        for i in 0..400 {
            gs.log_msg("kind", &format!("msg {i}"));
        }
        assert_eq!(gs.messages.len(), 400);
        assert_eq!(gs.messages[0].text, "msg 0");

        // Adding one more should drop "msg 0"
        gs.log_msg("kind", "msg 400");
        assert_eq!(gs.messages.len(), 400);
        assert_eq!(gs.messages[0].text, "msg 1");
        assert_eq!(gs.messages[399].text, "msg 400");
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
    fn remove_entity_clears_all_target_fields() {
        // eqoxide#331: killing the current target must clear ALL target-derived fields, not
        // just target_id — otherwise the HTTP /v1/observe/debug snapshot (which, unlike the HUD,
        // isn't gated on target_id being Some) keeps reporting the dead mob's name/HP forever.
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(10, "a rat", 0.0, 0.0, 0.0, true));
        gs.set_target(10);
        gs.target_con = Some([255, 0, 0]);
        gs.target_con_name = Some("red".to_string());
        gs.target_attitude = Some("scowls".to_string());
        assert_eq!(gs.target_name.as_deref(), Some("a rat"));

        gs.remove_entity(10);

        assert_eq!(gs.target_id, None);
        assert_eq!(gs.target_name, None, "must clear, not leak the dead mob's name");
        assert_eq!(gs.target_hp_pct, None, "must clear, not leak the dead mob's hp");
        assert_eq!(gs.target_con, None);
        assert_eq!(gs.target_con_name, None);
        assert_eq!(gs.target_attitude, None);
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

    // --- GameState::set_target (eqoxide#323: stale con/attitude/name/HP on re-target) ---

    #[test]
    fn set_target_unknown_spawn_clears_name_and_hp() {
        // Targeting a corpse / out-of-range spawn / bogus id not in `entities`: target_id
        // still updates, but name/HP must clear to None rather than keep the PREVIOUS
        // target's values (the actual #323 bug — target_id updated but name/hp didn't).
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "a rat", 0.0, 0.0, 0.0, true));
        gs.set_target(7);
        assert_eq!(gs.target_name.as_deref(), Some("a rat"));
        assert_eq!(gs.target_hp_pct, Some(100.0));

        gs.set_target(999_999); // not in entities (corpse / stale id)
        assert_eq!(gs.target_id, Some(999_999));
        assert_eq!(gs.target_name, None, "must clear, not keep the previous target's name");
        assert_eq!(gs.target_hp_pct, None, "must clear, not keep the previous target's hp");
    }

    #[test]
    fn set_target_clears_stale_con_attitude_on_retarget() {
        // A: target a dangerous mob, apply its consider reply (con/con_name/attitude set —
        // mirrors apply_consider), then immediately re-target a trivial mob. The old con MUST
        // NOT survive the re-target (it used to persist red until — or if the server never
        // considers the new target, e.g. a corpse — forever).
        //
        // NB: this test used to have a second "then_repopulates" half that assigned
        // gs.target_con = Some(X) and then asserted it equals Some(X) — a tautology that
        // asserted the implementation back to itself without ever calling apply_consider (which
        // lives in packet_handler.rs, owned elsewhere). Deleted rather than faked through. See
        // #354/#355 test-suite audit.
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(1, "a dragon", 0.0, 0.0, 0.0, true));
        gs.set_target(1);
        gs.target_con = Some([255, 0, 0]);
        gs.target_con_name = Some("red".to_string());
        gs.target_attitude = Some("scowls".to_string());

        gs.upsert_entity(make_entity(2, "a rat", 1.0, 0.0, 0.0, true));
        gs.set_target(2);
        assert_eq!(gs.target_con, None, "stale con must clear on re-target");
        assert_eq!(gs.target_con_name, None, "stale con_name must clear on re-target");
        assert_eq!(gs.target_attitude, None, "stale attitude must clear on re-target");
    }

    #[test]
    fn set_target_self_f1_resolves_player_name_and_hp_not_entities() {
        // F1 self-target: id == player_id. The player is never present in `entities`
        // (register_spawn skips the self-spawn), so this must NOT fall into the
        // "unknown spawn -> clear" branch — it must resolve from the player fields.
        let mut gs = GameState::new();
        gs.player_id = 1;
        gs.player_name = "Aldric".to_string();
        gs.hp_pct = 42.0;
        gs.set_target(1);
        assert!(!gs.entities.contains_key(&1), "player must never appear in entities");
        assert_eq!(gs.target_id, Some(1));
        assert_eq!(gs.target_name.as_deref(), Some("Aldric"));
        assert_eq!(gs.target_hp_pct, Some(42.0));
    }

    #[test]
    fn set_target_self_after_mob_clears_stale_con() {
        // Re-targeting SELF (F1) after having a con'd mob targeted must also clear the
        // stale con/attitude — self-target is never considered, so nothing else would.
        let mut gs = GameState::new();
        gs.player_id = 1;
        gs.player_name = "Aldric".to_string();
        gs.upsert_entity(make_entity(9, "a dragon", 0.0, 0.0, 0.0, true));
        gs.set_target(9);
        gs.target_con = Some([255, 0, 0]);
        gs.target_con_name = Some("red".to_string());
        gs.target_attitude = Some("scowls".to_string());

        gs.set_target(1); // F1
        assert_eq!(gs.target_con, None);
        assert_eq!(gs.target_con_name, None);
        assert_eq!(gs.target_attitude, None);
        assert_eq!(gs.target_name.as_deref(), Some("Aldric"));
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
        gs.hp_pct = 55.0; // seed a nonzero value so the assert actually exercises the update
        // max_hp=0 → uses max(1) guard; cur_hp=0 → 0%
        gs.update_hp(1, 0, 0);
        assert!((gs.hp_pct - 0.0).abs() < 1e-4);
    }

    // --- GameState::update_hp / update_hp_pct live-sync `target_hp_pct` (eqoxide#9, task 6) ---
    // target_hp_pct is a stored snapshot (seeded when a target is selected — see
    // ActionLoop::tick), not derived fresh from `entities` on every HUD read, so these HP
    // handlers must refresh it whenever the update is for whichever spawn is currently
    // targeted — including the F1 self-target case, where the player is never present in
    // `gs.entities` (register_spawn special-cases and skips the self-spawn).

    #[test]
    fn update_hp_refreshes_target_hp_pct_for_targeted_entity() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "mob", 0.0, 0.0, 0.0, true));
        gs.target_id = Some(7);
        gs.update_hp(7, 50, 200);
        let pct = gs.target_hp_pct.expect("target_hp_pct must be set for the targeted entity");
        assert!((pct - 25.0).abs() < 1e-4, "expected 25.0, got {pct}");
    }

    #[test]
    fn update_hp_leaves_target_hp_pct_untouched_for_non_targeted_entity() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "mob", 0.0, 0.0, 0.0, true));
        gs.upsert_entity(make_entity(8, "other mob", 0.0, 0.0, 0.0, true));
        gs.target_id = Some(8);
        gs.target_hp_pct = Some(99.0); // sentinel: whatever the targeted entity (8) last showed
        gs.update_hp(7, 50, 200); // HP update for a DIFFERENT, non-targeted entity
        assert_eq!(gs.target_hp_pct, Some(99.0), "target_hp_pct must not move for a non-targeted entity's HP update");
    }

    #[test]
    fn update_hp_pct_refreshes_target_hp_pct_for_targeted_entity() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "mob", 0.0, 0.0, 0.0, true));
        gs.target_id = Some(7);
        gs.update_hp_pct(7, 40.0);
        assert_eq!(gs.target_hp_pct, Some(40.0), "target_hp_pct must live-sync with a percent-only HP update for the targeted entity");
    }

    #[test]
    fn update_hp_pct_leaves_target_hp_pct_untouched_for_non_targeted_entity() {
        let mut gs = GameState::new();
        gs.upsert_entity(make_entity(7, "mob", 0.0, 0.0, 0.0, true));
        gs.upsert_entity(make_entity(8, "other mob", 0.0, 0.0, 0.0, true));
        gs.target_id = Some(8);
        gs.target_hp_pct = Some(99.0); // sentinel
        gs.update_hp_pct(7, 40.0); // percent-only update for a DIFFERENT, non-targeted entity
        assert_eq!(gs.target_hp_pct, Some(99.0), "target_hp_pct must not move for a non-targeted entity's percent-only HP update");
    }

    #[test]
    fn update_hp_self_target_refreshes_target_hp_pct_from_player_hp() {
        // F1 (self-target): target_id == player_id. The player is never present in
        // `entities` (register_spawn special-cases and skips the self-spawn), so this must
        // take the `spawn_id == self.player_id` branch and source target_hp_pct from the
        // player's own hp_pct field rather than `entities.get(&spawn_id)` (which would find
        // nothing and leave target_hp_pct stuck / unset).
        let mut gs = GameState::new();
        gs.player_id = 1;
        gs.target_id = Some(1);
        gs.update_hp(1, 30, 200); // 15%
        assert!(!gs.entities.contains_key(&1), "player must never appear in entities");
        let pct = gs.target_hp_pct.expect("target_hp_pct must be set for the self-target case");
        assert!((pct - 15.0).abs() < 1e-4, "expected 15.0, got {pct}");
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
            is_open: false,
        });
        gs.set_door_open(3, true);
        assert!(gs.doors.get(&3).unwrap().is_open);
        gs.set_door_open(3, false);
        assert!(!gs.doors.get(&3).unwrap().is_open);
        // Unknown door id is ignored, not a panic.
        gs.set_door_open(99, true);
        assert!(gs.doors.get(&99).is_none());
    }

    // --- TaskStatus and quest structures ---

    #[test]
    fn task_status_default_is_active() {
        use super::TaskStatus;
        let status = TaskStatus::default();
        assert_eq!(status, TaskStatus::Active);
    }

    #[test]
    fn group_member_level_resolves_from_profile_and_entities() {
        // OP_GroupUpdateB sends placeholder levels (70/65); the resolver ignores those and reads
        // the real level from the profile (self) or the member's spawn (others). (eqoxide#104)
        let mut gs = GameState::new();
        gs.player_name = "Me".into();
        gs.player_level = 12;
        let mut ally = make_entity(2, "Ally", 0.0, 0.0, 0.0, false);
        ally.level = 47;
        gs.upsert_entity(ally);
        assert_eq!(gs.group_member_level("Me"), 12, "self → player_level");
        assert_eq!(gs.group_member_level("Ally"), 47, "other in zone → entity level");
        assert_eq!(gs.group_member_level("OutOfZone"), 0, "unknown member → 0");
    }

}
