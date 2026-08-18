//! In-game state — player, entities, zone info, message log.

use std::collections::VecDeque;

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

/// An entity's **pose** — the discrete body-state the server publishes for a spawn
/// (`eq_constants.h` `Animation`: `Standing=100, Freeze=102, Looting=105, Sitting=110,
/// `Crouching=111, Lying=115`).
///
/// **#643 — why this is a type and not a `u32`.** `Entity` used to carry a single
/// `animation: u32` that was written from TWO semantically incompatible wire fields:
/// the spawn struct's pose byte (`stand_state`, values in the 100s) and the position
/// update's 10-bit **gait** sub-field (a speed code, roughly 0-40). Whichever packet
/// arrived last decided what the number meant, and the renderer's `_ => "idle"` catch-all
/// silently turned every unrecognised value into a plausible-looking default. Splitting
/// the two domains into two fields of two *different types* ([`Pose`] and [`Gait`]) makes
/// the mixed state unrepresentable: the gait writer cannot assign into a `Pose`, and the
/// pose writers cannot assign into a `Gait`, so neither can be misread as the other.
///
/// [`Pose::Unknown`] is deliberately explicit rather than being folded into `Standing`:
/// an unrecognised wire value is a thing the client genuinely does not know, and the
/// agent-honesty invariant says an observable "I don't know" beats a confident guess.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum Pose {
    /// `Animation::Standing` (100) — the ordinary upright state.
    #[default]
    Standing,
    /// `Animation::Freeze` (102) — held/frozen; renders as a still upright figure.
    Freeze,
    /// `Animation::Looting` (105) — kneeling over a corpse.
    Looting,
    /// `Animation::Sitting` (110).
    Sitting,
    /// `Animation::Crouching` (111) — ducked.
    Crouching,
    /// `Animation::Lying` (115) — prone / dead.
    Lying,
    /// A wire value outside the known enum. Carries the raw value so it can be reported
    /// verbatim instead of being guessed at. NEVER silently coerced to `Standing`.
    Unknown(u32),
}

impl Pose {
    /// Decode a wire pose code (spawn `stand_state`, or an `OP_SpawnAppearance` type-14
    /// `parameter`). Unrecognised values become [`Pose::Unknown`] — never a plausible default.
    pub fn from_wire(v: u32) -> Self {
        match v {
            100 => Pose::Standing,
            102 => Pose::Freeze,
            105 => Pose::Looting,
            110 => Pose::Sitting,
            111 => Pose::Crouching,
            115 => Pose::Lying,
            other => Pose::Unknown(other),
        }
    }

    /// The wire code this pose came from (round-trips `from_wire`).
    pub fn to_wire(self) -> u32 {
        match self {
            Pose::Standing  => 100,
            Pose::Freeze    => 102,
            Pose::Looting   => 105,
            Pose::Sitting   => 110,
            Pose::Crouching => 111,
            Pose::Lying     => 115,
            Pose::Unknown(v) => v,
        }
    }

    /// A stable, machine-readable label for API output. An unrecognised value is reported
    /// as `unknown(<raw>)` so a caller can SEE that the client could not interpret it.
    pub fn label(self) -> String {
        match self {
            Pose::Standing  => "standing".to_string(),
            Pose::Freeze    => "freeze".to_string(),
            Pose::Looting   => "looting".to_string(),
            Pose::Sitting   => "sitting".to_string(),
            Pose::Crouching => "crouching".to_string(),
            Pose::Lying     => "lying".to_string(),
            Pose::Unknown(v) => format!("unknown({v})"),
        }
    }
}


/// An entity's **gait** — the 10-bit `animation` sub-field of `OP_ClientUpdate`, a
/// speed-derived locomotion code (this client's own encoder is
/// `eqoxide_net::action_loop::speed_to_wire_animation`: ~12 at walk, 28 at full run).
///
/// A distinct newtype from [`Pose`] on purpose (#643) — see [`Pose`]'s doc comment. It is
/// `Option`al on [`Entity`] because an entity that has not yet sent a position update has
/// NO gait; `None` is "not reported yet", not "standing still".
/// The field is **signed** on the wire (`signed animation:10` in the RoF2 position bitfield), so a
/// mob backing up carries a negative gait. The decoder hands us the raw 10 bits unsigned, so
/// [`Gait::from_wire_10bit`] sign-extends: without it, gait `-12` (walking backwards) would be
/// reported as `1012`, a confident falsehood.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Gait(i32);

impl Gait {
    /// Sign-extend a raw unsigned 10-bit wire value into the signed gait it actually encodes.
    pub fn from_wire_10bit(raw: u32) -> Self {
        let v = (raw & 0x3FF) as i32;
        Gait(if v >= 512 { v - 1024 } else { v })
    }

    /// The signed gait code. Divide by 40 for units/second (see `speed_to_wire_animation`).
    pub fn raw(self) -> i32 { self.0 }
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
    /// Server-published body POSE (`Animation::Standing`, `Sitting`, `Crouching`, …).
    /// Written at spawn from `stand_state`, by `OP_SpawnAppearance` type 14 (the server's
    /// pose-change broadcast), and by `apply_death`. **Never** written from a position
    /// update — that carries [`Gait`], a different domain (#643).
    pub pose: Pose,
    /// Server-published GAIT (locomotion speed code) from the most recent `OP_ClientUpdate`.
    /// `None` until this entity has sent one — "not reported", not "stationary" (#643).
    pub gait: Option<Gait>,
    /// True for boat/ship races (`is_boat_race`): their wire z sits at the water surface with no
    /// server Z-offset (`Mob::FixZ` early-returns for `GetIsBoat`), and they're exempt from the
    /// render floor-snap so they don't sink (#194). Fixed for the entity's lifetime (race never
    /// changes) — unlike [`Entity::flymode`].
    pub is_boat: bool,
    /// The entity's CURRENT `flymode`/GravityBehavior wire code (Ground=0, Flying=1, Levitating=2,
    /// …). Seeded at spawn and **refreshed at runtime** by `OP_SpawnAppearance` type-19 (FlyMode) —
    /// so a mob that takes off or lands mid-session is reflected here, and every flymode-dependent
    /// decision (the wire-Z datum in [`Entity::floating`], the render floor-snap) is recomputed from
    /// the current value rather than frozen at spawn (#578). Before #578 this classification was a
    /// cached `floating: bool` that a later flymode change could not update — a stale, agent-facing
    /// Z falsehood.
    pub flymode: u8,
    /// Raw `NpcTintIndex` from the spawn wire (Spawn_Struct `npc_tint_id` on RoF2, eqoxide#857) — an
    /// ungated, DB-driven appearance channel distinct from `equipment_tint` (which only exists for
    /// playable races). Captured here so it is not silently discarded, but **deliberately NOT yet
    /// applied to rendering**: the native client's render-application algorithm for this value could
    /// not be established from source, and a read-only query of the live deployed `npc_types` table
    /// (2026-08-06) found 0 of 67530 rows set it nonzero, so nothing renders differently today either
    /// way. `0` is the documented "no tint" sentinel. See `register_spawn` in
    /// `eqoxide-net::packet_handler` for where this is wired from the wire decode.
    #[allow(dead_code)]
    pub npc_tint_index: u32,
}

impl Entity {
    #[allow(dead_code)]
    pub fn dist_to(&self, x: f32, y: f32, z: f32) -> f32 {
        ((self.x - x).powi(2) + (self.y - y).powi(2) + (self.z - z).powi(2)).sqrt()
    }

    /// The spawn/render "skips the server Z-offset & isn't floor-snapped" classification, **derived
    /// from the entity's CURRENT [`flymode`](Entity::flymode)** rather than stored — so a runtime
    /// flymode change (`OP_SpawnAppearance` type-19) is honored automatically, with no cached bool to
    /// go stale (#578). Boats and airborne (Flying/Levitating) mobs hover and carry no server
    /// Z-offset on their spawn z (#194/#548). NB: this is the SPAWN/RENDER rule (Levitating included);
    /// the POSITION-UPDATE Z conversion uses the stricter
    /// [`coord::position_update_skips_wire_z_offset`](crate::coord::position_update_skips_wire_z_offset),
    /// because a *patrolling* Levitating NPC's update z DOES carry the offset (#578 residual b).
    pub fn floating(&self) -> bool {
        crate::coord::skips_wire_z_offset(self.is_boat, self.flymode)
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

/// How much the server has actually disclosed about one task objective.
///
/// RoF2 sends `OP_TaskActivity` in **two different shapes that carry different amounts of truth**
/// (EQEmu `zone/task_manager.cpp:972` `SendTaskActivityShort` vs `zone/task_manager.cpp:989`
/// `SendTaskActivityLong` → `common/tasks.h:141` `SerializeObjective`). The short form is exactly
/// 25 bytes and contains **no** activity type, **no** target name and **no** counts at all — it is
/// what the real client renders as `???` for an activity that has not been unlocked yet.
///
/// Flattening both shapes into one struct with zeroed counts (what eqoxide did before #889) makes
/// an *undisclosed* objective indistinguishable from a real `0 of 0` one, and an agent reading
/// `/v1/quests/log` has no second channel to catch that. These variants make the confusion
/// unrepresentable: there is no `goal_count` field to read unless the server sent one.
///
/// Serialises flattened into the enclosing [`TaskActivity`] with a `"state"` discriminator, so a
/// locked objective is `{"activity_id":2,"state":"locked"}` — the count keys are *absent*, not
/// zero.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActivityProgress {
    /// Long form: the server disclosed the objective and its live progress.
    Known {
        /// EQEmu `TaskActivityType` (`common/tasks.h:46`) — declared `: int32_t`, and written with
        /// `WriteInt32` (`common/tasks.h:152`), so it is **signed** here too. 0 None, 1 Deliver,
        /// 2 Kill, 3 Loot, 4 SpeakWith, 5 Explore, 6 TradeSkill, 7 Fish, 8 Forage, 9 CastOn,
        /// 10 SkillOn, 11 Touch, 13 Collect, 100 GiveCash.
        ///
        /// **Type 100 (GiveCash) repurposes the two counts** (`common/tasks.h:143-150`): the
        /// server forces `goal_count` to 1 and sends `done_count` as a 0/1 *flag*, not a tally.
        /// The cash amount is not on the wire at all. Read `0/1` there as "not yet delivered",
        /// not as "0 of 1 items".
        activity_type: i32,
        /// `target_name` — the mob / item / location the step names.
        target:        String,
        /// `description_override` — server-authored replacement for the client's auto-generated
        /// objective sentence. Empty for most content.
        description:   String,
        done_count:    u32,
        goal_count:    u32,
        /// `ActivityInformation::optional` — an optional objective does not gate task completion.
        optional:      bool,
    },
    /// Short form (25 bytes): the activity exists but has not been unlocked. The server sent no
    /// type, no target and no counts, so neither does this — `optional` is genuinely all the
    /// 25 bytes carry beyond the ids.
    Locked {
        optional: bool,
    },
    /// The payload did not decode under either documented RoF2 form. Reported as such rather than
    /// guessed at, and never fatal — see `apply_task_activity`.
    Undecodable {
        /// Human-readable reason (which field ran out, or how many bytes were left over).
        reason: String,
    },
}

/// One objective/step of a Task-system quest (from `OP_TaskActivity`).
///
/// `activity_id` is always known (it is in both wire forms); everything else lives in
/// [`ActivityProgress`] because the short form does not carry it.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TaskActivity {
    pub activity_id: u32,
    #[serde(flatten)]
    pub progress:    ActivityProgress,
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

/// The self-player's Levitate (gravity-off) state, reconstructed from the TWO independent server
/// channels that carry it. (#529 Slice 1, extended by #586.)
///
/// ## Why this is a struct and not a `bool`
/// A plain `bool` had two unrelated writers racing on it, and the "bad" state (one writer's stale
/// belief clobbering the other's fresh truth) was trivially representable. Here each channel owns
/// its OWN field, nothing outside this type can write the answer, and the answer is *derived*:
///
/// - [`by_flymode`](Self::by_flymode) — the GravityBehavior channel. Set at zone-in from the
///   self-spawn's `flymode` byte (EQEmu `FillSpawnStruct` bakes `2` for a levitating client), and
///   cleared mid-zone by `OP_SpawnAppearance` type 19 (`AppearanceType::FlyMode`) `param=0` on buff
///   fade. This channel NEVER delivers a mid-zone SET: both the cast-on and the zone-in reapply call
///   `SendAppearancePacket(FlyMode, …, ignore_self=true)`.
/// - `by_buff` — the buff channel (#586). The buff slots that currently hold a spell our
///   `spells_us.txt` table says carries **SPA 57 (SE_Levitate)**. This is the channel the real RoF2
///   client uses for a mid-zone cast-on, and the only one that reports it promptly.
///
/// ## Precedence: OR, never overwrite
/// [`active`](Self::active) is `by_flymode || !by_buff.is_empty()`. Both channels are
/// server-authoritative and each only ever asserts what the server told us, so they cannot
/// disagree about a *positive*; OR means neither channel's silence can erase the other's evidence.
/// A fade clears both (the fade sends an `OP_Buff` fade AND the type-19 `param=0`), and a zone-in
/// [resyncs](Self::resync_on_zone_in) the buff channel from scratch so nothing carries over stale.
///
/// ## What is deliberately NOT modelled
/// Only "does this spell id carry SPA 57". No buff durations, no other SPA behavior, no bonus math.
/// An unknown spell id (not in the table, or no table loaded) never flips the levitate *belief* —
/// it is never guessed to be "not levitate". It IS recorded, though (see `unresolved` below), so the
/// answer can honestly report [`Levitating::Unknown`] instead of a confident `No`. See
/// [`crate::spells::SpellDb::has_effect`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LevitateState {
    /// Latest reading of the GravityBehavior channel (self-spawn `flymode` / type-19 appearance).
    by_flymode: bool,
    /// Buff slot ids currently known to hold a SPA-57 spell.
    by_buff: std::collections::BTreeSet<u32>,
    /// Buff slots the server told us about whose spell id we could NOT resolve to a SPA-57 answer
    /// (absent from the table, or no table loaded). While any such slot stands and no channel
    /// positively asserts levitate, the honest answer is [`Levitating::Unknown`] — a levitate could
    /// be hiding in a slot we cannot read (#598). A resolvable update for the slot, a fade, or a
    /// snapshot that omits/resolves it clears it. Never intersects `by_buff` (a slot is EITHER a
    /// known levitate OR unresolved, never both).
    unresolved: std::collections::BTreeSet<u32>,
}

/// The honest three-valued answer to "is the self-player levitating?", for the API boundary.
///
/// [`LevitateState::active`] must stay a `bool` — the controller has to make a concrete hover/fall
/// decision every frame. But an *observable* reported to a driving agent must never collapse "we
/// don't know" into a confident `false`: when a buff we were told about references a spell id our
/// table can't resolve (missing/truncated `spells_us.txt`) and no channel positively asserts
/// levitate, the truthful answer is neither `Yes` nor `No`. That is the exact silent-wrong-answer
/// the agent-honesty invariant exists to prevent (#598), so the API answer is three-valued and
/// `Unknown` is a distinct variant the caller can branch on — serialized as JSON `null`, never
/// `false` and never omitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Levitating {
    /// A channel positively asserts levitate (flymode 2/5, or a buff we KNOW carries SPA 57).
    Yes,
    /// Complete information, and no channel asserts levitate. A trustworthy negative.
    No,
    /// We received buff information we could not resolve and have no positive evidence, so we cannot
    /// honestly answer. Serialized as `null`.
    Unknown,
}

impl LevitateState {
    /// The one answer for the CONTROLLER: is the self-player levitating right now? A `bool` because
    /// the controller must pick hover-or-fall every frame; `Unknown` is treated as "don't hover on a
    /// guess" here, which is a safe physics default, NOT an honesty lie (this value is never reported
    /// to an agent — the API reads [`answer`](Self::answer)).
    pub fn active(&self) -> bool { self.by_flymode || !self.by_buff.is_empty() }

    /// The one answer for the API BOUNDARY: the honest three-valued levitate state (#598). A positive
    /// from any channel is `Yes`; otherwise an outstanding unresolvable buff makes it `Unknown` (we
    /// can't rule levitate out); only with complete, levitate-free information is it a confident `No`.
    pub fn answer(&self) -> Levitating {
        if self.active() { Levitating::Yes }
        else if !self.unresolved.is_empty() { Levitating::Unknown }
        else { Levitating::No }
    }

    /// Mid-zone `OP_SpawnAppearance` type 19 about our own spawn. In practice only ever the CLEAR
    /// (`param=0` on fade) — the 2/5 SET arm is defensive.
    pub fn set_flymode(&mut self, levitating: bool) { self.by_flymode = levitating; }

    /// Zone-in full resync from the self-spawn's `flymode` byte. That byte is the server's complete
    /// answer for this moment, so the buff channel is dropped rather than carried across the zone
    /// boundary (the zone-in buff snapshot repopulates it).
    pub fn resync_on_zone_in(&mut self, levitating: bool) {
        self.by_flymode = levitating;
        self.by_buff.clear();
        self.unresolved.clear();
    }

    /// One buff slot changed. `is_levitate` is the three-valued SPA-57 answer for the spell now in
    /// `slot` (`None` = we have no row for that spell id). `fading` = the server said this buff is
    /// going away.
    ///
    /// `None` never clears a slot we positively know holds a levitate (that would report "not
    /// levitating" while the character floats). Instead it RECORDS the slot as unresolved, so
    /// [`answer`](Self::answer) can honestly say `Unknown` rather than a confident `No` (#598).
    pub fn buff_slot_changed(&mut self, slot: u32, is_levitate: Option<bool>, fading: bool) {
        if fading {
            // A fade names the slot being vacated. Removing it is safe whatever the spell was: if
            // the slot wasn't in either set, this is a no-op.
            self.by_buff.remove(&slot);
            self.unresolved.remove(&slot);
            return;
        }
        match is_levitate {
            Some(true)  => { self.by_buff.insert(slot); self.unresolved.remove(&slot); }
            Some(false) => { self.by_buff.remove(&slot); self.unresolved.remove(&slot); }
            // Unresolvable: keep any known-levitate belief for this slot, but otherwise record that
            // we cannot read it — the difference between an honest `Unknown` and a confident `No`.
            None        => { if !self.by_buff.contains(&slot) { self.unresolved.insert(slot); } }
        }
    }

    /// A FULL buff-list snapshot (`OP_BuffCreate` with `all_buffs=1`): `(slot, is_levitate)` for
    /// every occupied slot. Slots absent from the snapshot, or holding a spell we KNOW is not
    /// levitate, drop out. A slot holding an UNKNOWN spell keeps a known-levitate belief but is
    /// otherwise recorded as unresolved — the snapshot is only as complete as our spell table, and we
    /// report that uncertainty (`Unknown`) rather than pretend the slot isn't levitate (#598).
    pub fn resync_from_snapshot(&mut self, slots: &[(u32, Option<bool>)]) {
        let mut next = std::collections::BTreeSet::new();
        let mut next_unresolved = std::collections::BTreeSet::new();
        for &(slot, is_levitate) in slots {
            match is_levitate {
                Some(true)  => { next.insert(slot); }
                Some(false) => {}
                None        => {
                    if self.by_buff.contains(&slot) { next.insert(slot); }
                    else { next_unresolved.insert(slot); }
                }
            }
        }
        self.by_buff = next;
        self.unresolved = next_unresolved;
    }

    /// Diagnostics: which channel(s) currently assert levitate. Not wire state — for logs/tests.
    pub fn from_flymode(&self) -> bool { self.by_flymode }
    /// Diagnostics: number of buff slots believed to hold a SPA-57 spell.
    pub fn levitate_buff_slots(&self) -> usize { self.by_buff.len() }
}

/// A single entry in the message log.
#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
    pub kind: String,
    pub text: String,
    pub timestamp: std::time::Instant,
    /// Any item/say links parsed out of `text` — the display text is already clean (hex-free);
    /// this carries the resolvable `item_id` alongside it (eqoxide#256).
    pub item_links: Vec<ItemLink>,
}

impl Default for LogEntry {
    fn default() -> Self {
        LogEntry {
            kind: String::new(),
            text: String::new(),
            timestamp: std::time::Instant::now(),
            item_links: Vec::new(),
        }
    }
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

/// Server-authoritative, **Model-written-only** world state (MVC increment C1, #451).
///
/// This is the *world as the server sees it*: the zone the server placed us in, its environment
/// (safe point, underworld floor, distance fog), and the live contents of that zone (spawns,
/// doors, exit points). Every field here is written EXCLUSIVELY by the Model — the `eq-net`
/// thread — from inbound server packets (OP_NewZone, OP_ZoneSpawns, OP_SpawnDoor,
/// OP_SEND_ZONE_POINTS, …). No View (render or HTTP) ever writes it.
///
/// ## The Model-only invariant (checkable)
/// A View cannot mutate `WorldState` because a View never holds a `&mut GameState`: the net
/// thread owns the one mutable pre-publish `GameState`, mutates `world`, and publishes an
/// **immutable** `Arc<GameState>` snapshot each tick (`gameplay::publish_snapshot` → `ArcSwap`);
/// render/HTTP read it lock-free via `load_full`, holding only a shared `Arc<GameState>` (#343).
/// So the ONLY `&mut WorldState` in the program is reachable solely from net-thread code — the
/// property "WorldState is Model-only" is enforced by ownership, not convention, and is grep-
/// checkable: every `.world.<field> =` write site is in the `eq_net` module.
///
/// ## NOT in here: the client prediction
/// The local player's *predicted* position is deliberately NOT a `WorldState` field — see the
/// `player_x/y/z/heading` doc on [`GameState`]. That is client-side prediction owned by the
/// render `CharacterController`, not server truth.
///
/// C2 (#452) migrates the remaining Model-written `GameState` fields (player facts, combat,
/// spells, tasks, merchants, groups, chat, loot, …) into here using this same boundary; C1
/// establishes the type + the zone/world-contents cluster + the named prediction split.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct WorldState {
    // Zone
    pub zone_name: String,
    pub zone_id: u16,
    pub zone_changed: bool,
    /// #335/agent-honesty: the last zone-entry handshake (an in-game zone change) TIMED OUT — we
    /// connected to the new zone server and sent OP_ZoneEntry, but never got OP_NewZone → … →
    /// OP_SendExpZonein back within the deadline (the `zone-entry-handshake-race.md` never-accepted
    /// case). Set true only on that failure; `zone_name` is CLEARED alongside it so no agent reads the
    /// OLD zone as the current one. Reset to false at the start of every zone-in by `begin_zone_in`.
    /// Without this a wedged zone-in kept reporting `connected: true` + the previous `zone_name` — a
    /// confident falsehood (#343/#470 anti-pattern).
    pub zone_in_failed: bool,
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
    /// (see `~/git/eq_kb/zone-distance-fog.md`). RoF2's `NewZone_Struct` carries
    /// 4 fog "slots"; only slot 0 (the DB's un-suffixed fog_* columns) is populated by ordinary
    /// zone content, so we only read that one (see the KB doc's "Semantics of the 4 slots" note).
    pub zone_fog: Option<ZoneFog>,
    /// Server world clock from OP_TimeOfDay (eqoxide#561). `None` until the first OP_TimeOfDay of
    /// the session arrives (sent once during zone-entry, and again on a GM/quest world-time change);
    /// the render side extrapolates the live time-of-day from it each frame (1 EQ-min = 3 real sec)
    /// to drive the sky gradient. Honest: never a faked value — while `None`, the sky renders a
    /// documented daytime default rather than inventing a "current" time. See `eqoxide_core::sky`.
    pub eq_clock: Option<crate::sky::EqClock>,
    /// Server-authoritative weather from OP_Weather (eqoxide#542). Defaults to clear; the renderer
    /// draws a rain/snow particle field around the camera scaled by intensity, and nothing at all
    /// when clear. Model-only, single-writer (the net thread's OP_Weather handler). Honest: reflects
    /// the REAL server weather — a short/invalid packet is dropped, never a fabricated storm. See
    /// `eqoxide_core::weather`.
    pub weather: crate::weather::WeatherState,
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

    // Zone exit points (populated by OP_SEND_ZONE_POINTS on zone entry)
    pub zone_points: Vec<ZonePoint>,
}

/// Declares [`ControllerHoldReason`] and derives [`ControllerHoldReason::ALL`] from the SAME token
/// list — the [`crate::game_state::ControllerHoldReason`] doc explains why it is not a hand-written
/// array. Mirrors `eqoxide_ipc`'s `net_thread_end!` (#890 B3), which is where the measurement that
/// motivates it was taken.
macro_rules! controller_hold_reason {
    (
        $(#[$enum_meta:meta])*
        pub enum $name:ident { $($(#[$variant_meta:meta])* $variant:ident),+ $(,)? }
    ) => {
        $(#[$enum_meta])*
        pub enum $name { $($(#[$variant_meta])* $variant),+ }

        impl $name {
            /// Every variant, in declaration order — DERIVED from the enum, never maintained
            /// beside it. #884's HTTP suites iterate this so a new hold reason cannot ship with
            /// its movement-endpoint behaviour unasserted.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];
        }
    };
}

controller_hold_reason! {
/// Why the render `CharacterController` is holding the body still — a predicament it cannot leave
/// under its own power, and which no other published field reveals (#724).
///
/// Both variants are the SAME shape: a recovery path decided it must not let the body go where the
/// physics was taking it, looked for a banked "last good" position to put it at instead, and found
/// none. #724 clears the banked ring on every position discontinuity, which makes an empty ring the
/// NORMAL post-relocation state and these holds an ordinary outcome of a GM summon into rock.
///
/// For `EmbeddedNoRecovery` (#845) the hold means **the ZONE has nowhere to stand**, not that this
/// body has no memory: that branch first falls through to a zone-wide search. It is not, however, a
/// hold that clears itself — both directions were measured. A SUCCEEDING search never publishes this
/// at all, because the search arm returns before the hold is raised (0 held frames of 300, in a zone
/// the search solves); a hold that IS published does not clear on its own, because in a zone whose
/// geometry does not change the once-a-second retry keeps failing for the same reason (1800 frames /
/// 60 s, raised and never cleared). So a published hold is the signature of a FAILED search and it
/// still takes something external to end it. See `movement.rs`'s `nearest_standing_place`; that
/// nothing published marks the relocation is #925.
///
/// `UnderworldNoRecovery` has no such search, on purpose: it runs after collide-and-slide, so
/// lateral driver input still reaches the body and the state is not absorbing — see
/// `last_resort_placement`'s doc for why extending the search there was reverted.
///
/// This is deliberately NOT "am I stuck?" in general — a character walking into a wall is blocked
/// and is not this. It is specifically "the controller has stopped the body and has no way to
/// resume", which is the state an agent would otherwise read as a perfectly healthy stand-still.
///
/// # `ALL` is derived from this declaration, not maintained beside it
///
/// The [`controller_hold_reason!`] wrapper below emits the enum and [`ControllerHoldReason::ALL`]
/// from one token stream. #890 measured why that matters: a hand-written `[NetThreadEnd; 5]` in a
/// test module did not know about a sixth variant, so a variant wired to exactly the silent
/// inheritance those tests exist to forbid shipped fully green. #884's `every_movement_endpoint_…`
/// suites iterate `ALL`, and a hold reason they do not iterate is a hold reason whose HTTP
/// behaviour is unasserted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControllerHoldReason {
    /// The body cannot be placed: the client's embedded test is a DISJUNCTION — geometry pierces
    /// the footprint, **or** there is no floor within `GROUND_DEPTH` (200 u) below the feet — and
    /// this variant covers both. (#845's live casualty was the second: a column with zero triangles
    /// over it. The name says "embedded"; do not read it as "geometry is inside the body".) The
    /// depenetration push-out ring found nowhere it can occupy, there is no banked good position to
    /// fall back to, and — since #845 — the zone-wide last-resort search also found nowhere. While
    /// it lasts, `depenetrate` returns `true` every frame, so the whole rest of the step is skipped:
    /// the body cannot move at all, in any direction, under any driver (WASD, `/goto`, `/move`).
    EmbeddedNoRecovery,
    /// The #150 fall-through guard refused a descent to/below the zone's underworld floor and had
    /// no banked good position to restore. The body hangs at the height it reached, not falling and
    /// not landing. Lateral movement still works; the body is out of the world vertically.
    UnderworldNoRecovery,
}
}

/// **What a [`ControllerHoldReason`] does to DRIVEN motion (#884).** Not "how bad is this hold" —
/// the single question every movement endpoint has to answer before it may claim it accepted work:
/// *can the driver input this request produces reach the body at all?*
///
/// It is a type rather than a `bool` on the hold, and rather than prose in a `detail` string, for
/// the reason #890 recorded about `NetThreadEnd`: a consumer that wants the distinction from prose
/// must either match on the text (which any reword silently breaks) or collapse it to `is_some()`
/// (which loses it entirely, and answers "no motion" for a hold under which lateral motion works).
/// The `match` in [`ControllerHoldReason::motion`] is exhaustive, so a third hold reason cannot be
/// added without its author stating which of these it is.
///
/// **Both mappings are read off the controller's own control flow, not inferred from the reason's
/// name.** See `src/movement.rs`: `step` calls `depenetrate` first and `return`s immediately if it
/// handled the frame, while the #150 fall-through guard that raises `UnderworldNoRecovery` runs
/// *after* collide-and-slide has already applied the frame's lateral wish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeldMotion {
    /// **No driver input reaches the body.** `depenetrate` returns `true` on every frame of this
    /// hold and `step` returns before it reads `intent` at all, so a wish of any shape — lateral,
    /// vertical, jump, from WASD or from any HTTP endpoint — produces no motion. Measured live on
    /// #845/#884: thirteen client-API calls, position byte-identical throughout.
    AllFrozen,
    /// **A lateral wish still reaches the body.** The hold is raised at the end of a frame that ran
    /// collide-and-slide, so `/v1/move/manual` can walk this body out from under the hold — which
    /// is exactly why `last_resort_placement` was deliberately NOT extended to this arm. Says
    /// nothing about vertical wishes or jumps: this variant is the warrant for *not refusing* a
    /// movement request, never a promise that a particular one will work.
    LateralStillReaches,
}

impl ControllerHoldReason {
    /// Stable machine token for the API. Never reword these — agents match on them.
    pub fn as_str(self) -> &'static str {
        match self {
            ControllerHoldReason::EmbeddedNoRecovery   => "embedded_no_recovery",
            ControllerHoldReason::UnderworldNoRecovery => "underworld_no_recovery",
        }
    }

    /// What this hold does to driven motion (#884). See [`HeldMotion`] for where each answer comes
    /// from; adding a variant to [`ControllerHoldReason`] is a compile error until this states it.
    pub fn motion(self) -> HeldMotion {
        match self {
            ControllerHoldReason::EmbeddedNoRecovery   => HeldMotion::AllFrozen,
            ControllerHoldReason::UnderworldNoRecovery => HeldMotion::LateralStillReaches,
        }
    }
}

/// A hold in force RIGHT NOW, with how long it has lasted.
///
/// Level-triggered by construction: the controller clears this at the top of every `step` and only
/// a branch that is actively holding the body *this frame* re-sets it, so it cannot outlive the
/// condition it describes. That clear-path is the point — an observable that latches on and never
/// clears is its own honesty bug (#343/#679), not a fix for one.
///
/// # `secs` defeats `publish_snapshot`'s dedup for the hold's whole duration — deliberately
///
/// This type's `PartialEq` reaches [`GameState`]'s, which `eqoxide_net::gameplay::publish_snapshot`
/// uses to skip republishing an unchanged snapshot. `secs` advances on every stepped frame, so a
/// held body compares unequal on every net tick and stores a fresh `Arc` each time, for as long as
/// the hold lasts (#724 round-2 review, N6).
///
/// **Nothing downstream amplifies that, and here is why.** The only consumer of the snapshot `Arc`'s
/// *identity* is `App::poll_external`, which sets `activity = true` on `!Arc::ptr_eq` to wake the
/// render loop. That same function already sets `activity = true` unconditionally on `!on_ground`,
/// which every `underworld_no_recovery` hold guarantees (its arm runs only inside `if
/// !self.on_ground`) — so on that shape the republish cannot wake a loop that was not already
/// awake. HTTP reads the snapshot by value (`.load()`), never by identity, and no endpoint or
/// long-poll waits on a pointer change. The residual cost is one `GameState::clone` per net tick
/// instead of a pointer compare, bounded per tick and unbounded only in duration.
///
/// Excluding `secs` from equality was considered and rejected: it would make two genuinely different
/// states compare equal, so a hold that changed duration could go unpublished, and the field exists
/// precisely so an agent can watch it advance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ControllerHold {
    pub reason: ControllerHoldReason,
    /// Seconds this hold has been continuously in force (controller frame time, accumulated only
    /// while the reason is unchanged). A change of reason restarts it at one frame.
    pub secs: f32,
}

/// All state the renderer needs for one frame.
///
/// `PartialEq` is load-bearing: `eq_net::gameplay::publish_snapshot` compares the freshly-mutated
/// `GameState` against the last-published snapshot and only stores a new `Arc` when it actually
/// changed. That makes the published Arc's pointer identity a complete "did anything happen"
/// signal — the render loop's `poll_external` (app.rs) wakes on ANY network-thread mutation
/// (inbound packet OR a client-initiated HTTP request handled by `ActionLoop::tick`), and a
/// genuinely idle world lets the event loop sleep instead of spinning.
///
/// ## MVC structure (increment C1, #451)
/// `GameState = { world: WorldState (Model-only server truth) , the client PREDICTION , … }`.
/// `world` (see [`WorldState`]) is the server-authoritative world the Model writes and Views only
/// read. The `CommandState` write-path facade (Phase A) is the other half of the conceptual
/// `GameState = { WorldState, CommandState }` split, but it lives OUTSIDE this snapshot on purpose
/// — it is the View→Model *input* path (held in the `ipc` bundles, drained by `ActionLoop`), not
/// published render state, so it is intentionally not a field here.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct GameState {
    // Player
    pub player_id: u32,
    pub player_name: String,
    /// ## CLIENT-SIDE PREDICTION — deliberately NOT [`WorldState`] (MVC C1, #451)
    /// The local player's position/heading is the render `CharacterController`'s *predicted*
    /// local state, NOT server-authoritative world truth. The controller (render side, `app.rs`)
    /// owns the prediction and streams it to the net thread, which MIRRORS it into these fields
    /// (`ActionLoop::stream_position`). Server truth arrives as a correction: a large server-pushed
    /// jump is handed back to the controller via `pos_correction` → `CharacterController::teleport`
    /// (client-side-prediction / server-reconciliation). Because the physics that writes these is
    /// SPLIT — authoritative (server) vs predicted (render controller the Model reconciles) — they
    /// stay OUT of `world`, so "`WorldState` is Model-only server truth" is literally true. (In the
    /// offline `--testzone` path the render loop, as sole owner, writes them from the controller
    /// directly — see `app.rs`.)
    pub player_x: f32,
    pub player_y: f32,
    pub player_z: f32,
    /// #513 (agent-honesty): has our position for the CURRENT zone actually been established yet?
    ///
    /// `player_x/y/z` are plain `f32`, so they always hold SOME number — that number just isn't
    /// always trustworthy. On the very first zone-in of a session they read the struct's
    /// construction-time `0.0`. On every zone change after that, [`GameState::begin_zone_in`]
    /// deliberately does **NOT** reset them (see that fn's doc) — they keep holding the PREVIOUS
    /// zone's last-known coordinates until something in the new zone overwrites them. That second
    /// case is the more dangerous falsehood: a `distance` computed from a stale
    /// old-zone position is a *plausible*-looking number, unlike an obviously-absurd
    /// origin-relative one, so a consumer has no way to smell it out without this flag. Consumers
    /// that would otherwise publish a fabricated figure must gate on this and report an honest
    /// "unknown" instead — exactly the just-zoned-in window where #513's original wrong-target
    /// near-miss happened.
    ///
    /// Set by any path that establishes `player_x/y/z` for the CURRENT zone — ALL of the
    /// following, not just one:
    ///   - self `OP_ClientUpdate` (`packet_handler.rs`) — server-authoritative.
    ///   - bind-point respawn (`apply_bind_respawn`, `packet_handler.rs`) — server-authoritative.
    ///   - a same-zone teleport (`gameplay.rs`) — server-driven destination.
    ///   - `MockModel::publish_snapshot` (`model.rs`) — offline/testzone, where the Model itself
    ///     IS the position authority, so its published position is real by definition.
    ///   - the controller mirror in `ActionLoop::stream_position` (`action_loop.rs`) — the
    ///     controller's own PREDICTED position, on the normal (non-correction) path, once the
    ///     controller has actually been placed in this zone. This one is deliberate, not a leak:
    ///     keying the flag on self `OP_ClientUpdate` alone was measured live to omit `distance`
    ///     ALWAYS rather than only while genuinely unknown, because the server rarely sends it.
    ///
    /// A genuine server correction inside `stream_position` hands the new position to the
    /// controller and returns WITHOUT setting this flag — within `stream_position`, it only flips
    /// once a later tick adopts that position on the normal path (though another handler entirely
    /// — self `OP_ClientUpdate`, respawn, same-zone teleport — may already have set it first). If a
    /// new zone's spawn point happens to land within the correction threshold of the last-streamed
    /// old-zone position, the correction is skipped entirely and the flag flips on that
    /// stale-but-close controller position instead; the resulting `distance` error is bounded by
    /// that same threshold in X/Y only (the correction check ignores Z) — see #593(c) for why that
    /// horizontal residual window is harmless rather than a falsehood.
    ///
    /// Reset to false by [`GameState::begin_zone_in`] on every zone change.
    pub player_pos_known: bool,
    /// **#543/#660: `player_x/y/z` (and possibly `world.zone_id`) are the client's own GUESS.**
    /// `Some(when)` from the moment a zone-line crossing applies a locally-derived position — the
    /// ADVERTISED arrival of the zone point we walked into, written before the server has said
    /// anything, so the character leaves the trigger region and the cross does not re-fire.
    ///
    /// Cleared the instant the SERVER tells us where we are (`player_pos_known` set from a server
    /// packet — a self-position update, a respawn, a server-driven teleport). Deliberately NOT
    /// cleared by the OP_ZoneChange echo: the echo settles WHICH ZONE, and `world.zone_id` flips
    /// there, but the position does not arrive until the new zone's first update. That gap is
    /// exactly the observed falsehood (#660 review B2): `/v1/observe/debug` served
    /// `zone: "qeynos"` with a qeynos2 `pos` — well-formed, confident, mutually inconsistent, and
    /// unmarked. It is marked now.
    ///
    /// Client-side writers (nav, the HTTP move API) do NOT clear it — they are not evidence about
    /// where the server thinks we are.
    pub position_provisional_since: Option<std::time::Instant>,
    /// #713 item 1 — auto-cross attempts during the current continuous stand on zone-line geometry
    /// that did not result in a crossing. `None` = the character is not on (or has just left) a
    /// region it has tried to cross. See [`crate::zone_cross::CrossAttempts`] for why this counts
    /// attempts rather than server denials AND why the tally is per stand rather than per region
    /// index, and [`crate::zone_cross::MAX_CROSS_ATTEMPTS`] for the bound and its reasoning.
    ///
    /// **Deliberately NOT a [`WorldState`] field.** It is a record of what THIS CLIENT decided to
    /// do, not server truth — same reasoning as `position_provisional_since` above. Written only by
    /// `eqoxide_net::action_loop`'s standing auto-cross; read by it (the gate) and by
    /// `GET /v1/observe/debug` (the `zone_cross_stopped` disclosure), both through
    /// `CrossAttempts::blocks`, so the gate and its observable cannot disagree.
    ///
    /// Cleared on the FIRST net-thread tick on which the standing probe finds the character off
    /// every zone-line region, and by [`GameState::begin_zone_in`] (region indices are a per-zone
    /// namespace). That reset sits OUTSIDE the auto-cross cooldown guard on purpose: inside it, the
    /// attempt that reaches the bound stamps the cooldown itself, so a walk-off/walk-on shorter than
    /// the cooldown was sampled ON the line at both ends and cleared nothing at all. See
    /// `ActionLoop::drain_zone_cross`.
    pub zone_cross_attempts: Option<crate::zone_cross::CrossAttempts>,
    /// #713 item 2 — what the most recently drained `POST /v1/move/zone_cross` decided to do, and
    /// in particular whether it degraded to the #683 best-effort fallback (walk to a line whose
    /// destination only the server knows). `None` = no request has been resolved to a zone line in
    /// this zone. See [`crate::zone_cross::ZoneCrossPlan`].
    ///
    /// Also client-side-decision state rather than server truth, so also not in [`WorldState`].
    /// Cleared at the start of every resolution (a request that located no line leaves no stale
    /// plan) and by [`GameState::begin_zone_in`].
    pub zone_cross_plan: Option<crate::zone_cross::ZoneCrossPlan>,
    /// **#724 review B1: the render controller is holding the body still and cannot resume.**
    /// `None` = it is not (which includes "it is simply standing still because nothing asked it to
    /// move"); `Some` = a recovery path has frozen the body and has nothing to restore it onto. See
    /// [`ControllerHold`] for the two shapes and for why this cannot latch.
    ///
    /// Mirrored here from `ControllerView::hold` by `ActionLoop::stream_position`, exactly like
    /// `player_x/y/z` and with exactly the same freshness: it is as current as the last render frame
    /// that stepped the controller. If the render loop is not stepping, the controller is not moving
    /// the body either and this holds its last computed value — the position beside it is stale in
    /// the same breath and by the same amount.
    ///
    /// **Two measured exceptions to that last clause (#846), because it was written as unqualified
    /// and is not.**
    ///
    /// 1. **A server correction.** On a tick where `stream_position` detects one (a GM `#summon`, a
    ///    knockback, an anti-cheat snap), it hands the jump to the render thread through
    ///    `ipc::PosCorrection` and returns EARLY, so `player_x/y/z` are already the SERVER's new
    ///    coordinates while the controller is still frozen where it was. That branch therefore
    ///    WITHDRAWS this field (and `player_afloat_stall`) rather than leave the pair as a fresh
    ///    position beside an old predicament. The withdrawal is not the net thread inventing an
    ///    answer: the correction it just handed over is consumed by `CharacterController::teleport`,
    ///    which drops the hold and the afloat window UNCONDITIONALLY, on every path through it (the
    ///    function is straight-line, with no early return). So the withdrawal publishes the
    ///    disclosure the controller itself holds the moment it adopts. It is not a promise about the
    ///    render thread's next publication — a summon into geometry publishes `Some(..)` next frame,
    ///    about the NEW position — and the argument that carries it is the direction: withdrawing
    ///    can only lose a warning for a tick. Pinned, on BOTH halves of the pair, by
    ///    `a_re_asserted_summon_never_pairs_a_fresh_pos_with_a_stale_hold_846` in `eqoxide-net`.
    ///    Do NOT read the mismatch this closes as a bounded ~10 ms window: measured against a server
    ///    that re-asserts the correction every tick while the render loop idles, it recurred on every
    ///    other tick indefinitely, which is why the branch withdraws rather than waits.
    /// 2. **A zone-in.** [`GameState::begin_zone_in`] below clears this field on the net thread,
    ///    because a hold describes collision geometry the zone-in has just dropped. That clear only
    ///    works when it also reaches the `ControllerView` the mirror reads — see that method's doc
    ///    and `eqoxide_ipc::ControllerSlots::begin_zone_in`.
    ///
    /// So the net thread CAN change this field, in exactly those two places, and in both it
    /// WITHDRAWS. What it cannot do — property-tested by
    /// `no_net_tick_can_free_or_manufacture_a_hold_846` in `eqoxide-net` — is put a hold here that
    /// the render thread did not publish. That test freezes the view (an idle render loop, modelled
    /// exactly) across a matrix of server repositions on both sides of the correction threshold;
    /// `the_hold_mirror_tracks_the_render_thread_over_time_846` beside it varies the other axis,
    /// the render thread republishing over time, so a mirror that latches and never withdraws goes
    /// red rather than green.
    ///
    /// **What none of that bounds is how long the render loop may stay idle.** That coupling lives
    /// in `app.rs`'s `poll_external` (a pending `pos_correction`, or any `GameState` change at all,
    /// marks the loop active) and it is UNGUARDED: that call site needs a GPU and a window, and
    /// neutering the condition was measured to leave the whole workspace green (#846). It is the
    /// latency bound, not the honesty guarantee.
    ///
    /// This is a CLIENT-SIDE physics fact, not server truth. It deliberately does not live in
    /// [`WorldState`]: the server has no opinion about it and would happily agree with the position
    /// we keep streaming from inside the rock.
    pub player_hold: Option<ControllerHold>,
    /// **#776/#801 (agent-honesty): the body is afloat, a driver is asking it to swim somewhere, and
    /// it is not getting there.** `None` = it is not in that state, which includes every ordinary
    /// floating character — a body nobody is wishing at never opens a window at all. See
    /// [`crate::afloat::AfloatStall`].
    ///
    /// **This is NOT a [`ControllerHold`] and must never be folded into one.** A hold asserts the
    /// body cannot move at all, under any driver. This asserts only that *this wish* has produced no
    /// motion for this long: the body may well be escapable by a different drive (a driven dive out
    /// of a pocket mouth is the worked case). Publishing it as a hold would be a new false claim,
    /// not a fix for the old silence — which is why it is a separate field with a separate type and
    /// a separate key on the API.
    ///
    /// Mirrored here from `ControllerView::afloat_stall` by `ActionLoop::stream_position`, on the
    /// same tick and with the same freshness as `player_hold` and `player_x/y/z` beside it.
    /// Cleared by [`GameState::begin_zone_in`] for the same reason `player_hold` is: the render loop
    /// may publish nothing at all across a ~10 s zone load, and a stall describes a body failing to
    /// cross *specific geometry* that no longer exists. Also a CLIENT-SIDE physics fact, so also
    /// deliberately not in [`WorldState`].
    pub player_afloat_stall: Option<crate::afloat::AfloatStall>,
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
    /// #529/#586: the self-player's Levitate (gravity off — the character free-floats/hovers over
    /// land rather than falling). Server-authoritative and derived from two independent wire
    /// channels; read it via [`GameState::player_levitating`], never by poking at a bool. The render
    /// controller mirrors it each frame via `CharacterController::set_levitating`.
    pub levitate: LevitateState,
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
    ///
    /// NOT necessarily a figure the server sent — read [`hp_verified`](Self::hp_verified) beside it
    /// (#1005). No path computes DAMAGE into these fields any more, but a few still write them from
    /// the client's own inference; `unverified_hp_writes` below lists every one.
    pub cur_hp: i32,
    pub max_hp: i32,
    /// True once at least one authoritative self `OP_HPUpdate` has landed (#1005). It is the only
    /// server message carrying BOTH the player's current and maximum HP, so it is the only reading
    /// that can establish the whole published `hp`/`hp_max`/`hp_pct` triple. Before the first one,
    /// `cur_hp`/`max_hp` are the all-zero startup default or a PlayerProfile seed whose max is a
    /// guess — neither is server truth, and publishing either as confirmed is the #1005 defect.
    /// Set ONLY by [`update_hp`](Self::update_hp) for `player_id`; never by an estimate path.
    pub hp_confirmed: bool,
    /// Count of writes to the player's `cur_hp`/`hp_pct` made by the CLIENT rather than read from a
    /// self `OP_HPUpdate`, since the last such update (#1005). Nonzero means the published HP is at
    /// least partly the client's own inference. The writers that increment it, all of them:
    ///
    /// * the `OP_Death` zeroing of `cur_hp`/`hp_pct` — the death itself is authoritative, the
    ///   *number* zero is the client's inference from it;
    /// * the bind-respawn "real EQ revives at full HP" assumption (eqoxide#68);
    /// * the PlayerProfile HP seed (eqoxide#19), whose `cur_hp` IS server-sent but which carries no
    ///   max at all — so `max_hp` is seeded = `cur_hp` and the published `hp_pct` is then 100 for a
    ///   character that zoned in wounded. That is a second unmarked estimate derived from `cur_hp`,
    ///   so the seed counts here too.
    ///
    /// Deliberately a counter, not a bool, and [`hp_verified`](Self::hp_verified) is a COMPUTED read
    /// of it — never an independently-settable field — for the same reason `coin_verified` is
    /// (#361): a single call site must not be able to assert a trust the client has not actually
    /// verified against server truth. Reset to zero ONLY by [`update_hp`](Self::update_hp) for
    /// `player_id`. Saturating, so a long fight can never wrap it back to "verified".
    pub unverified_hp_writes: u32,
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

    /// Server-authoritative, Model-written-only world state — the zone the server placed us in,
    /// its environment, and its live contents (spawns/doors/exits). See [`WorldState`]. Views read
    /// it; only the `eq-net` Model writes it. (MVC C1, #451 — this is the WorldState boundary.)
    pub world: WorldState,

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
    // `UcsInfo` is a core-local POD (moved down in #544 Step 2b); the wire parser lives in
    // `eq_net::ucs`, which re-exports this same type.
    pub ucs: Option<crate::ucs::UcsInfo>,

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
    /// ~/git/eq_kb/loot-protocol.md). While this is `Some`, `loot_current_corpse`
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
    /// Set true the first time OP_CharInventory has been applied — **including a legitimate
    /// 0-item inventory**. Distinguishes "the server sent an inventory and it's empty" from
    /// "we're still waiting for the server to send one" (eqoxide#695, agent-honesty invariant):
    /// without this flag, `inventory.is_empty()` alone can't tell a completed-empty load from a
    /// pending one, and the UI showed a permanent false "(waiting for inventory...)" for any
    /// character with a genuinely empty inventory.
    pub inventory_received: bool,

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
    /// Run/walk toggle (#625): `true` = run, `false` = walk. Governs the LOCAL movement speed the
    /// controller/nav-walker drives at (`eqoxide_core::physics::RUN_SPEED` vs `WALK_SPEED`), and
    /// mirrors the last `OP_SetRunMode` (0x009f) this client sent — the server itself never echoes
    /// this flag back (no ack packet exists for it), so this is our own send-time intent, exactly
    /// like `sitting`/`auto_attack` above track the last posture/attack request we sent, not a
    /// server confirmation. Defaults to `true` (running) in [`GameState::new`], matching this
    /// client's behavior before #625: every driver always moved at `RUN_SPEED`.
    pub run_mode: bool,
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
            // #625: default to running, matching this client's behavior before the run/walk toggle
            // existed (every driver always moved at RUN_SPEED). Plain `derive(Default)` would give
            // `false` (walk) — deliberately overridden here, the same way `messages` overrides its
            // derived-empty-`VecDeque` default just above.
            run_mode: true,
            ..Default::default()
        }
    }

    /// Start a zone-server session (login zone handoff, or an in-game zone change): purge the
    /// previous zone's spawns and doors and re-arm the once-per-zone-in OP_NewZone apply. Called at
    /// the top of each zone-entry handshake, before OP_ReqClientSpawn asks for the spawn stream, so
    /// the clear can never race the stream it precedes. (#322)
    ///
    /// Does **NOT** reset `player_x/y/z` — they keep holding the previous zone's last-known
    /// coordinates (there is nothing else to set them to yet). What it DOES do is clear
    /// [`GameState::player_pos_known`] to `false`, so consumers know those stale numbers are not
    /// yet trustworthy for the new zone — see that field's doc.
    ///
    /// **Net-thread callers: call `eqoxide_ipc::ControllerSlots::begin_zone_in` instead of this
    /// (#846 review B1).** The two controller disclosures cleared below are MIRRORED into this
    /// struct from a `ControllerView` that lives above this crate, by an unconditional write on
    /// every ~10 ms net tick. Clearing them here without invalidating that view was measured to
    /// survive exactly one tick before the departed zone's hold came back — which is the opposite of
    /// what the comments on those two lines claim to achieve. This function cannot reach the view
    /// itself (`eqoxide-core` sits below `eqoxide-ipc`), so the pairing lives there.
    pub fn begin_zone_in(&mut self) {
        self.world.entities.clear();
        self.world.doors.clear();
        // #513: the previous zone's coordinates say nothing about where we are in the new one, so
        // our position is UNKNOWN again until the new zone server tells us. Anything deriving a
        // distance from it must report an honest unknown until then, not a figure measured from
        // the old zone's numbers or the origin.
        self.player_pos_known = false;
        // #543/#660: a crossing marks the position PROVISIONAL (the advertised guess); once the
        // crossing turns out to be a real zone change that guess is moot and the honest state is
        // `player_pos_known = false`, not "provisional". Verified live that without this clear the
        // marker stayed `true` in the new zone for 30s+ while `pos` was already correct.
        self.position_provisional_since = None;
        // #713: both zone-cross facts are about the zone we are LEAVING, and both key on per-zone
        // namespaces (region index, advertised destination zone id). See their field docs.
        self.zone_cross_attempts = None;
        self.zone_cross_plan = None;
        // #724/#776/#801: a hold and an afloat stall both describe collision geometry — the stall
        // by naming an ANCHOR in the departed zone's coordinate frame — that this zone-in drops.
        //
        // ⚠️ NEITHER CLEAR IS SUFFICIENT ON ITS OWN, and could not be: the values they race live in
        // `ControllerView`, above this crate, and `ActionLoop::stream_position` mirrors that view
        // into these fields unconditionally every ~10 ms net tick, so clearing only here was
        // measured to get the departed zone's hold back on the very next tick. Net-thread callers
        // must go through `eqoxide_ipc::ControllerSlots::begin_zone_in`, which pairs these with the
        // view clear. (`clear_hold` on `app.rs`'s not-stepped frames covers the render loop that
        // keeps rendering through the load; this covers the one that publishes nothing at all.)
        self.player_hold = None;
        self.player_afloat_stall = None;
        // The target belongs to the zone we just left: its spawn id is meaningless in the new zone
        // and #270 already purges `entities`, so target_id would point at a gone spawn while
        // target_name/target_hp_pct fall back to the stale cached snapshot — /observe/debug then
        // reports a full-HP target from the OLD zone (a confident falsehood an agent may attack /
        // consider). Clear the whole target (id + name + hp + con) here, not just the entity map (#408).
        self.clear_target();
        // #883: `last_consider` is the SAME hazard as the target fields above — spawn ids are a
        // per-zone namespace, so the same id in the new zone is a different mob at a different
        // difficulty, while `ago_secs` keeps counting normally and discloses nothing. It needs its
        // own line because `clear_target()` deliberately does NOT touch it (it is spawn-scoped, not
        // target-scoped), which is how the clear stayed missing from #336 to #883.
        self.last_consider = None;
        self.world.new_zone_applied = false;
        // #683 review (F2): the previous zone's advertised zone points are meaningless — and
        // actively dangerous — in the new zone. Left in place they persist through the
        // OP_NewZone→OP_SendZonepoints window under the NEW zone_id, where they can satisfy the
        // unresolved-cross gates (`classify_unresolved_cross`) with another zone's data: its
        // "zone points have arrived" premise would be met by adverts that say nothing about THIS
        // zone. Cleared here, the list is empty until the new zone's own OP_SendZonepoints lands
        // (`apply_zone_points` rebuilds it), which is the honest state for that window.
        self.world.zone_points.clear();
        // A fresh zone-in attempt: clear any prior handshake-failure flag so it reflects only THIS
        // attempt (#335). The flag is re-raised by `run_zone_entry_handshake` only if this one times
        // out. `zone_name` is deliberately NOT cleared here — it stays showing the zone we came from
        // for the renderer's loading screen; a FAILED handshake clears it (there, honestly).
        self.world.zone_in_failed = false;
        // A cast cannot survive a zone change: the spawn ids, the cast bar and every packet that
        // would have explained the cast belong to the zone we just left. Carrying `casting` across
        // would report a cast in flight that can never end, and carrying `suppress_cast_end` would
        // eat the terminal of the first cast in the NEW zone. (eqoxide#348 review)
        self.reset_cast_tracking();
        self.casting = None;
        // ── #941: the rest of the spawn/session-scoped state ──────────────────────────────────
        // Every field below names either a spawn id in the zone we just left (a per-zone namespace,
        // exactly like `target_id`/`last_consider` above) or an open NPC window that the departed
        // NPC's disappearance has already closed. See `zone_scoped_state_941` on the class guard
        // test for the question each was audited against.
        //
        // ⚠️ Three of these have a PUBLISHED copy this function cannot reach, the same way
        // `world.doors` does (#891/#934 review B1): `dialogue_choices` is mirrored into
        // `InteractSlots::dialogue`, `merchant_open`/`merchant_items` into
        // `MerchantSlots::merchant`, and `task_offers` into `QuestSlots::task_offers_shared` —
        // all three only by `ActionLoop::sync_messages`/`sync_merchant`/`sync_tasks`, whose sole
        // caller is `run_gameplay_phase`'s packet drain — NOT the zone-entry handshake's own
        // drain. `gameplay::run_zone_entry_handshake` clears those three slots alongside this
        // call; clearing only here leaves GET /v1/observe/dialogue, GET /v1/merchant/list and
        // GET /v1/quests/offers serving the departed zone's answers for the whole zone load.

        // The clickable saylink choices of the NPC we were talking to. Directly served by
        // GET /v1/observe/dialogue and directly ACTIONABLE via POST /v1/interact/dialogue, which
        // sends an OP_ItemLinkClick — so a surviving list is a well-formed, plausible, false answer
        // that an agent can then act on, against an NPC that is not in this zone. Its only other
        // writers are "a new NPC line carried saylinks" (`apply_*` in `packet_handler`) and the
        // hail clear (#274), so without this it survives until one of those happens to fire.
        self.dialogue_choices.clear();
        // An open task-select window: each offer names the OFFERING NPC's spawn id (`npc_id`,
        // "required by `OP_AcceptNewTask`'s `task_master_id` field" per `TaskOffer`'s own doc) —
        // a per-zone namespace, exactly like `trainer_open`/`merchant_open` below. Directly served
        // by GET /v1/quests/offers and directly ACTIONABLE via POST /v1/quests/accept, which
        // resolves `task_master_id` from this list and sends OP_AcceptNewTask to it — the same
        // read-then-act shape as `dialogue_choices` above. Its only other writer is
        // `apply_task_select_window` replacing the list wholesale on a fresh OP_TaskSelectWindow
        // from THIS zone's NPC, so without this clear a stale offer (and its stale `npc_id`)
        // survives indefinitely, not just for the zone-in window.
        self.task_offers.clear();
        // An open guildmaster-training window: the trainer's spawn id plus the caps IT offers.
        // Exposed as `player.trainer_open`/`player.trainer_skills` on /v1/observe/debug and read by
        // POST /v1/trainer/train, which addresses OP_GMTrainSkill to `trainer_open` — a stale id
        // aims a training request at whatever the new zone assigned that spawn id.
        self.trainer_open = None;
        self.trainer_skills.clear();
        // An open merchant window: the merchant's spawn id and its wares. Composes with — rather
        // than fights — `begin_shop_open_for`'s no-flicker guard (#361 review FIX 2): that guard
        // exists to stop a pre-buy/pre-sell OP_ShopRequest RESEND against the merchant that is
        // already open from flickering the window closed for a round-trip. A zone-in is not a
        // resend; the window is genuinely gone, and leaving it set is what would be the lie. After
        // this clear the guard's next comparison is `None != Some(new_id)`, its clear-and-reopen
        // path, which is exactly right for a first open in a new zone.
        self.merchant_open = None;
        self.merchant_items.clear();
        // The player's pet's spawn id. The pet itself follows the player across a zone line in EQ,
        // but the NEW zone assigns it a NEW spawn record, so this id is stale from the moment we
        // zone. It is normally dropped by `remove_entity` when the pet despawns — which
        // `begin_zone_in` cannot go through, because it empties `world.entities` wholesale. Until
        // the new zone's pet spawn packet lands and rewrites it, a stale id sends /v1/pet/command's
        // OP_PetCommands at a spawn that is not our pet.
        self.pet_id = None;
        // The whole auto-loot session and its queue. Every id in it is a CORPSE spawn id in the
        // departed zone; an ungated `pending_loot` front would make the net thread's own drain send
        // this zone's OP_LootRequest for a corpse id that named someone else's mob.
        //
        // ⚠️ #414's "deliberately left as-is" note on `loot_session_active`/`loot_current_corpse`
        // (see `gameplay.rs`'s `LootTickAction::TimedOut` arm and `apply_loot_open_timeout`) does
        // NOT govern here, and this clear is not an override of it. That note keeps the session
        // pinned so a LATE `OP_MoneyOnCorpse`/`OP_LootComplete` — neither of which carries a corpse
        // id — cannot be misattributed to the NEXT corpse's session. A zone change ends the zone
        // server session that could still deliver such an ack (`run_gameplay_phase` drops the
        // stream and `EqStream::connect`s a new one before the handshake that calls this), so there
        // is no late ack left to quarantine AGAINST; keeping the quarantine armed would instead
        // carry the departed zone's block into this one and withhold the new zone's first
        // OP_LootRequest until it times out.
        self.pending_loot.clear();
        self.loot_session_active = false;
        self.loot_confirmed = false;
        self.loot_current_corpse = None;
        self.loot_last_activity = None;
        self.loot_end_requested_at = None;
        self.loot_queued_at = None;
        self.loot_defensive_close_at = None;
        // Both are HashMaps keyed by spawn id. Neither is HTTP-exposed; both feed behaviour.
        // `combat_anims` also keys the PLAYER's own id, which is the one id that does NOT change
        // across a zone line — so an un-cleared entry replays the departed zone's swing on the
        // first frames of the new one. `recent_attackers` feeds the auto-combat add-retarget; it is
        // separately TTL-pruned (`ATTACKER_TTL`, 6s, in `action_loop`) and its consumer additionally
        // requires the id to resolve to a live reachable NPC, so the surviving harm is narrow — an
        // id the new zone reuses inside that window being treated as something that attacked us.
        // Cleared anyway: "narrow" is a bound on a wrong answer, not an argument for keeping it.
        self.combat_anims.clear();
        self.recent_attackers.clear();
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
            self.world.entities.values().find(|e| e.name == name).map(|e| e.level).unwrap_or(0)
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
        self.world.entities.insert(e.spawn_id, e);
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

    /// True only when the published self-HP triple (`hp`/`hp_max`/`hp_pct`) is exactly what the
    /// server last reported: an authoritative self `OP_HPUpdate` has landed (`hp_confirmed`) AND no
    /// client-side write has touched it since (`unverified_hp_writes == 0`). #1005, agent-honesty.
    ///
    /// Computed — never an independently-settable field — so no single estimate path can assert a
    /// trust the client has not verified against server truth. Same construction as
    /// [`coin_verified`](Self::coin_verified) (#361).
    ///
    /// It governs `target_hp_pct` too whenever the player is self-targeted (F1): that field then
    /// resolves from `self.hp_pct`, so it carries the same estimate.
    ///
    /// Deliberately CONSERVATIVE in the safe direction: it reads false during the window between a
    /// PlayerProfile seed and the first `OP_HPUpdate`, because the profile carries no max and the
    /// seeded `hp_pct` is therefore derived from a guess. Under-claiming is survivable; the
    /// forbidden direction is publishing client arithmetic as a confirmation.
    pub fn hp_verified(&self) -> bool {
        self.hp_confirmed && self.unverified_hp_writes == 0
    }

    /// Record that the published self-HP has just been written by the CLIENT rather than read from
    /// a self `OP_HPUpdate` (#1005), so [`hp_verified`](Self::hp_verified) reads false until the
    /// next authoritative update reconciles it. Saturating: a marathon fight cannot wrap the
    /// counter back to "verified".
    pub fn mark_hp_estimated(&mut self) {
        self.unverified_hp_writes = self.unverified_hp_writes.saturating_add(1);
    }

    /// Is the self-player levitating (gravity off)? The CONTROLLER read path (bool: hover or fall).
    /// The single read path for [`LevitateState`]. Agent-facing observables must instead use
    /// [`player_levitating_state`](Self::player_levitating_state), which distinguishes `Unknown`.
    pub fn player_levitating(&self) -> bool { self.levitate.active() }

    /// The honest three-valued levitate state for the API boundary (#598): `Yes`/`No`/`Unknown`.
    /// `Unknown` when a buff we were told about references a spell id our table can't resolve and no
    /// channel positively asserts levitate — so the agent never reads a confident `false` we can't
    /// back up. See [`Levitating`].
    pub fn player_levitating_state(&self) -> Levitating { self.levitate.answer() }

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
        self.world.entities.remove(&spawn_id);
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
    /// `gs.world.entities` (a corpse, an out-of-range spawn, a stale/bogus id): the previous
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
        } else if let Some(e) = self.world.entities.get(&id) {
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
        self.world.doors.insert(d.door_id, d);
    }

    /// Apply a server door-state change. Unknown door ids are ignored.
    pub fn set_door_open(&mut self, door_id: u8, open: bool) {
        if let Some(d) = self.world.doors.get_mut(&door_id) {
            d.is_open = open;
        }
    }

    /// Apply an AUTHORITATIVE self `OP_HPUpdate` (or an entity's HP update). For `player_id` this
    /// is the one and only path that marks the published HP server-verified — it resets
    /// `unverified_hp_writes` to zero and sets `hp_confirmed` (#1005).
    ///
    /// **Never call this with a client-derived figure.** Doing so publishes the client's own
    /// arithmetic as server truth, which is exactly the #1005 defect; use
    /// [`update_hp_estimated`](Self::update_hp_estimated) instead.
    pub fn update_hp(&mut self, spawn_id: u32, cur_hp: i32, max_hp: i32) {
        self.write_hp(spawn_id, cur_hp, max_hp, true);
    }

    /// Same write as [`update_hp`], for a figure the CLIENT derived rather than read off the wire
    /// (#1005) — today, the bind-respawn "real EQ revives at full HP" assumption. Leaves the HP
    /// unverified, so `hp_verified()` keeps reading false until a real `OP_HPUpdate` reconciles it.
    pub fn update_hp_estimated(&mut self, spawn_id: u32, cur_hp: i32, max_hp: i32) {
        self.write_hp(spawn_id, cur_hp, max_hp, false);
    }

    fn write_hp(&mut self, spawn_id: u32, cur_hp: i32, max_hp: i32, from_server: bool) {
        if spawn_id == self.player_id {
            self.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
            self.cur_hp = cur_hp;
            self.max_hp = max_hp;
            // #1005: a server OP_HPUpdate is the ONLY thing that can clear the estimate debt — it
            // is the only message carrying both cur and max. A client-derived figure adds to it.
            if from_server {
                self.hp_confirmed = true;
                self.unverified_hp_writes = 0;
            } else {
                self.mark_hp_estimated();
            }
            // Alive again → clear the death/respawn bookkeeping. (eqoxide#61, #50)
            if cur_hp > 0 {
                self.player_dead = false;       // revived / healed above 0
                self.player_dead_since = None;  // clear the respawn safety-net timer
            }
        } else if let Some(e) = self.world.entities.get_mut(&spawn_id) {
            e.cur_hp = cur_hp;
            e.max_hp = max_hp;
            e.hp_pct = (cur_hp as f32 / max_hp.max(1) as f32) * 100.0;
        }
        // Keep the target HUD's HP gauge live: target_hp_pct is a stored snapshot (seeded
        // when the target is selected — see ActionLoop::tick), not derived fresh from
        // `entities` on every read, so it must be refreshed here whenever the update is for
        // whichever spawn is currently targeted (mob or self via F1). (eqoxide#9, task 6)
        if self.target_id == Some(spawn_id) {
            self.target_hp_pct = (spawn_id == self.player_id).then_some(self.hp_pct)
                .or_else(|| self.world.entities.get(&spawn_id).map(|e| e.hp_pct));
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
            if let Some(e) = self.world.entities.get_mut(&spawn_id) {
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
            .world
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

/// Turn an entity key like "Guard_Phaeton000" into a display name "Guard Phaeton". Relocated from
/// `eqoxide-http` (#544 Step 2o): it's a pure string helper with zero deps, used both by the HTTP
/// layer (targeting/looting/merchant/trainer replies) and by `eqoxide-ui` (NPC dialogue, nearby-NPC
/// action labels) — putting it here, in the leaf both already depend on, lets `eqoxide-ui` avoid an
/// up-reference into `eqoxide-http` for a single display-formatting function. `eqoxide-http`
/// re-exports it (`pub use eqoxide_core::game_state::clean_entity_name;`) so every existing
/// `crate::clean_entity_name` / `crate::http::clean_entity_name` call site keeps resolving unchanged.
pub fn clean_entity_name(raw: &str) -> String {
    raw.trim_end_matches(|c: char| c.is_ascii_digit())
        .replace('_', " ")
        .trim()
        .to_string()
}

/// Test-only entity constructor. Gated on `test-fixtures` (not bare `#[cfg(test)]`) and `pub` so the
/// app crate's own tests can build fixture entities across the crate boundary — core's `#[cfg(test)]`
/// is invisible downstream (#544 Step 2b; the region_map fixture pattern). Call it as
/// `eqoxide::game_state::make_entity(...)` (i.e. `crate::game_state::make_entity` in the app crate).
#[cfg(any(test, feature = "test-fixtures"))]
pub fn make_entity(id: u32, name: &str, x: f32, y: f32, z: f32, is_npc: bool) -> Entity {
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
        pose: Pose::Standing, gait: None, is_boat: false, flymode: 0, npc_tint_index: 0,
    }
}

#[cfg(test)]
mod pose_tests_643 {
    use super::{Gait, Pose};

    /// #643: every `Animation::*` value from `eq_constants.h` must round-trip, and anything else
    /// must land in `Unknown` carrying its raw value — NOT be folded into a plausible default.
    #[test]
    fn pose_round_trips_and_keeps_unknown_values_verbatim() {
        for v in [100u32, 102, 105, 110, 111, 115] {
            assert_eq!(Pose::from_wire(v).to_wire(), v, "pose {v} must round-trip");
            assert!(!matches!(Pose::from_wire(v), Pose::Unknown(_)), "{v} is a known pose");
        }
        assert_eq!(Pose::from_wire(199), Pose::Unknown(199));
        assert_eq!(Pose::from_wire(199).label(), "unknown(199)");
        assert_eq!(Pose::from_wire(12).label(), "unknown(12)",
            "a GAIT value arriving on the pose channel reads as unknown, never as 'standing'");
    }

    /// #643 review: EVERY label is pinned individually, and they are pinned to be pairwise
    /// DISTINCT. Without this, collapsing two poses onto one label is invisible — the reviewer
    /// demonstrated exactly that by making `Freeze` and `Looting` both report `"standing"` and
    /// watching the whole suite stay green. That is this PR's own bug class one layer up: `label`
    /// is the agent-facing string, so a collapsed label is the client confidently reporting a
    /// state that is not true, on the very field added for honesty. `Looting` (105) in particular
    /// is reachable in ordinary play — a looting character must never read as `standing`.
    ///
    /// The label is also deliberately NOT the renderer's clip name: `crouching` and `sitting`
    /// happen to coincide today, but `looting`/`freeze` report themselves while `scene.rs` draws
    /// them with the idle clip, and `lying` reports itself while the clip is `dead`. Asserting the
    /// full table here keeps that separation explicit instead of accidental.
    #[test]
    fn every_pose_label_is_exact_and_pairwise_distinct() {
        let table = [
            (Pose::Standing,  "standing"),
            (Pose::Freeze,    "freeze"),
            (Pose::Looting,   "looting"),
            (Pose::Sitting,   "sitting"),
            (Pose::Crouching, "crouching"),
            (Pose::Lying,     "lying"),
        ];
        for (pose, want) in table {
            assert_eq!(pose.label(), want, "{pose:?} must report exactly {want:?}");
        }
        // Wire-code → label, through the real decode path an agent's value actually takes.
        assert_eq!(Pose::from_wire(100).label(), "standing");
        assert_eq!(Pose::from_wire(102).label(), "freeze");
        assert_eq!(Pose::from_wire(105).label(), "looting");
        assert_eq!(Pose::from_wire(110).label(), "sitting");
        assert_eq!(Pose::from_wire(111).label(), "crouching");
        assert_eq!(Pose::from_wire(115).label(), "lying");

        let mut labels: Vec<String> = table.iter().map(|(p, _)| p.label()).collect();
        labels.push(Pose::Unknown(7).label());
        let n = labels.len();
        labels.sort();
        labels.dedup();
        assert_eq!(labels.len(), n,
            "two poses collapsed onto one label — an agent could not tell them apart");
    }

    /// The wire field is `signed animation:10` (RoF2 position bitfield), and the decoder hands us
    /// raw unsigned bits. Without sign extension a mob walking BACKWARDS at -12 would be reported
    /// as 1012 — a well-formed, confident falsehood.
    #[test]
    fn gait_sign_extends_the_signed_10bit_wire_field() {
        assert_eq!(Gait::from_wire_10bit(0).raw(), 0);
        assert_eq!(Gait::from_wire_10bit(12).raw(), 12);   // native walkspeed
        assert_eq!(Gait::from_wire_10bit(28).raw(), 28);   // full run
        assert_eq!(Gait::from_wire_10bit(511).raw(), 511); // max positive
        assert_eq!(Gait::from_wire_10bit(512).raw(), -512); // min negative
        assert_eq!(Gait::from_wire_10bit(1012).raw(), -12); // walking backwards
        assert_eq!(Gait::from_wire_10bit(1023).raw(), -1);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{CastState, ControllerHold, ControllerHoldReason, DialogueChoice, Door, GameState,
                HeldMotion, LastConsider, MerchantItem, TaskOffer, ZonePoint, make_entity};

    /// #586/#598: exhaustive property over every ordering of the levitate channels' events —
    /// including the FULL-SNAPSHOT (`resync_from_snapshot`) path that carries the real mid-zone
    /// cast-on (#586 established it arrives as an `OP_BuffCreate` snapshot with `all_buffs=1`). #586's
    /// original alphabet omitted snapshots, so the bug-carrying path had only example coverage (#598
    /// finding 3); it is now first-class here.
    ///
    /// Two invariants, both against an independent reference model:
    /// - `active()` is EXACTLY "some channel currently holds POSITIVE evidence" — never a stale
    ///   `true` after all evidence is withdrawn, never a `false` while evidence stands, never
    ///   dependent on which channel spoke last.
    /// - `answer()` (the agent-facing tri-state) is `Yes` iff `active()`, else `Unknown` iff an
    ///   unresolvable buff stands, else `No` — so an unknown state can NEVER surface as a confident
    ///   `No` (#598 finding 1). MUTATION-CHECK: make `answer()` return `No` in place of `Unknown` and
    ///   this goes RED on the snapshot/unknown-buff orderings.
    #[test]
    fn levitate_state_is_exactly_the_union_of_its_two_channels() {
        use super::{LevitateState, Levitating};
        use std::collections::BTreeSet;
        #[derive(Clone, Copy, Debug)]
        enum Ev { Fly(bool), BuffOn(u32), BuffOff(u32), BuffKnownOther(u32), BuffUnknown(u32),
                  ZoneIn(bool), SnapLev, SnapOther, SnapUnknown }
        let alphabet = [
            Ev::Fly(true), Ev::Fly(false),
            Ev::BuffOn(1), Ev::BuffOff(1), Ev::BuffKnownOther(1), Ev::BuffUnknown(1),
            Ev::BuffOn(2), Ev::BuffOff(2), Ev::BuffUnknown(2),
            Ev::ZoneIn(true), Ev::ZoneIn(false),
            Ev::SnapLev, Ev::SnapOther, Ev::SnapUnknown,
        ];
        // A snapshot RECOMPUTES both belief sets from scratch over exactly the listed slots.
        let ref_snapshot = |prev: &BTreeSet<u32>, snap: &[(u32, Option<bool>)]| {
            let (mut ns, mut nu): (BTreeSet<u32>, BTreeSet<u32>) = Default::default();
            for &(slot, isl) in snap {
                match isl {
                    Some(true)  => { ns.insert(slot); }
                    Some(false) => {}
                    None        => { if prev.contains(&slot) { ns.insert(slot); } else { nu.insert(slot); } }
                }
            }
            (ns, nu)
        };
        // Every sequence of length 3 over the 14-symbol alphabet (2744 orderings).
        for a in alphabet { for b in alphabet { for c in alphabet {
            let seq = [a, b, c];
            let mut st = LevitateState::default();
            let mut ref_fly = false;
            let mut ref_slots: BTreeSet<u32> = Default::default();
            let mut ref_unresolved: BTreeSet<u32> = Default::default();
            for ev in seq {
                match ev {
                    Ev::Fly(v)   => { st.set_flymode(v); ref_fly = v; }
                    Ev::ZoneIn(v) => { st.resync_on_zone_in(v); ref_fly = v; ref_slots.clear(); ref_unresolved.clear(); }
                    Ev::BuffOn(s) => { st.buff_slot_changed(s, Some(true), false); ref_slots.insert(s); ref_unresolved.remove(&s); }
                    Ev::BuffKnownOther(s) => { st.buff_slot_changed(s, Some(false), false); ref_slots.remove(&s); ref_unresolved.remove(&s); }
                    Ev::BuffOff(s) => { st.buff_slot_changed(s, None, true); ref_slots.remove(&s); ref_unresolved.remove(&s); }
                    // An UNKNOWN spell id never flips the levitate belief, but IS recorded so the
                    // answer can honestly say Unknown.
                    //
                    // #742: both sides of this arm used to hardcode slot 1 and DISCARD the variant's
                    // payload, which is what the `dead_code` warning on `BuffUnknown`'s field was
                    // pointing at. The alphabet therefore could not express "an unresolvable buff in
                    // a slot OTHER than the one the rest of the sequence acts on": adding
                    // `BuffUnknown(2)` produced a symbol byte-identical in effect to
                    // `BuffUnknown(1)`, silently, while reading as extra coverage. The slot now comes
                    // from the event on BOTH the state under test and the reference model, so the
                    // symbol means what it says.
                    Ev::BuffUnknown(s) => { st.buff_slot_changed(s, None, false);
                                            if !ref_slots.contains(&s) { ref_unresolved.insert(s); } }
                    Ev::SnapLev    => { st.resync_from_snapshot(&[(1, Some(true))]);
                                        let (ns, nu) = ref_snapshot(&ref_slots, &[(1, Some(true))]); ref_slots = ns; ref_unresolved = nu; }
                    Ev::SnapOther  => { st.resync_from_snapshot(&[(1, Some(false))]);
                                        let (ns, nu) = ref_snapshot(&ref_slots, &[(1, Some(false))]); ref_slots = ns; ref_unresolved = nu; }
                    Ev::SnapUnknown => { st.resync_from_snapshot(&[(1, None)]);
                                        let (ns, nu) = ref_snapshot(&ref_slots, &[(1, None)]); ref_slots = ns; ref_unresolved = nu; }
                }
                let ref_active = ref_fly || !ref_slots.is_empty();
                assert_eq!(st.active(), ref_active,
                           "levitate active() mismatch after {seq:?} (fly={ref_fly} slots={ref_slots:?})");
                let ref_answer = if ref_active { Levitating::Yes }
                                 else if !ref_unresolved.is_empty() { Levitating::Unknown }
                                 else { Levitating::No };
                assert_eq!(st.answer(), ref_answer,
                           "levitate answer() mismatch after {seq:?} (fly={ref_fly} slots={ref_slots:?} unresolved={ref_unresolved:?})");
            }
        }}}
    }

    /// #586: a full buff-list snapshot may only conclude "no levitate" from spells it actually
    /// knows. A slot holding an id absent from our table keeps whatever we already believed.
    #[test]
    fn levitate_snapshot_never_erases_belief_on_an_unknown_spell() {
        use super::LevitateState;
        let mut st = LevitateState::default();
        st.buff_slot_changed(3, Some(true), false);
        assert!(st.active());
        // Snapshot lists slot 3 with a spell we cannot resolve → belief preserved.
        st.resync_from_snapshot(&[(3, None)]);
        assert!(st.active(), "unknown spell in a snapshot must not erase a known-true levitate");
        // Snapshot lists slot 3 with a spell we KNOW is not levitate → cleared.
        st.resync_from_snapshot(&[(3, Some(false))]);
        assert!(!st.active(), "a known non-levitate spell in that slot does clear it");
        // A slot we never believed in, holding an unknown spell, is not invented as levitate.
        st.resync_from_snapshot(&[(9, None)]);
        assert!(!st.active(), "an unknown spell must never be guessed as levitate either");
    }

    /// #598 finding 1 — the honesty bug this issue exists for: an UNKNOWN levitate state must reach
    /// the API boundary as an explicit `Unknown`, never a confident `No`. A missing/truncated spell
    /// table makes every buff unresolvable, so a levitate cast on us mid-zone leaves `active()` false
    /// while we genuinely cannot rule levitate out. `answer()` must distinguish that from a real
    /// negative.
    ///
    /// MUTATION-CHECK (this test is RED unless the fix stands): make `LevitateState::answer` return
    /// `Levitating::No` instead of `Levitating::Unknown` (the pre-#598 collapse) → the Unknown
    /// assertions below fail. On unmodified `origin/main` this test does not exist and does not
    /// compile (there is no `answer()`/`Levitating`), so its discriminator is this mutation.
    #[test]
    fn unresolvable_buff_answers_unknown_not_a_confident_no() {
        use super::{LevitateState, Levitating};
        let mut st = LevitateState::default();
        // Cold start: no evidence, nothing unresolved → a trustworthy negative.
        assert_eq!(st.answer(), Levitating::No, "fresh state with full information is a real No");

        // A buff arrives whose spell id our table cannot resolve (table missing/truncated). We have
        // NO positive evidence — but we also cannot say we're not levitating: it could be hiding in
        // this slot. The honest answer is Unknown, and `active()` (the controller bool) stays false.
        st.buff_slot_changed(4, None, false);
        assert!(!st.active(), "an unresolved buff is not positive evidence of levitate");
        assert_eq!(st.answer(), Levitating::Unknown,
                   "an unresolvable buff with no positive evidence is UNKNOWN, never a confident No");

        // The same via the real mid-zone cast-on path — a full buff-list snapshot we can't resolve.
        let mut st2 = LevitateState::default();
        st2.resync_from_snapshot(&[(4, None)]);
        assert_eq!(st2.answer(), Levitating::Unknown,
                   "an unresolvable buff in a SNAPSHOT is UNKNOWN too (the real cast-on path)");

        // Positive evidence dominates: even alongside an unresolved slot, a known levitate is Yes.
        st.buff_slot_changed(1, Some(true), false);
        assert_eq!(st.answer(), Levitating::Yes, "a known-SPA-57 buff is a definite Yes");

        // Resolving the slot to a KNOWN non-levitate, and clearing the levitate, returns a real No.
        st.buff_slot_changed(4, Some(false), false);
        st.buff_slot_changed(1, None, true);
        assert_eq!(st.answer(), Levitating::No,
                   "once every slot is resolved and levitate-free, it is a confident No again");
    }

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

    /// #1005 — the client must never publish a self-HP figure the server did not send WITHOUT
    /// marking it. This drives the update path directly rather than a live run: a live run showing
    /// correct HP proves the path CAN be right and cannot discharge a "never" claim.
    ///
    /// The measured defect was a client-side DAMAGE subtraction, and that arithmetic is now gone
    /// outright — `apply_combat_damage` no longer touches `cur_hp`, and the assertion that it
    /// cannot lives beside it in `eqoxide-net`. What remains here is the residue: paths that still
    /// write the player's own HP from a client INFERENCE (a death's zero, a respawn's assumed full
    /// bar, a zone-in seed with no max). Those cannot be deleted — the fields have to hold
    /// something — so they must be distinguishable instead, which is what this covers.
    #[test]
    fn a_client_derived_self_hp_write_can_never_read_verified_1005() {
        let mut gs = GameState::new();
        gs.player_id = 7;

        // The all-zero startup default is not a server reading either. `hp_confirmed` is what stops
        // an untouched GameState publishing 0/0 as confirmed — the counter alone would say verified.
        assert!(!gs.hp_verified(),
            "an untouched GameState has heard nothing from the server; 0/0 is not a confirmation");

        gs.update_hp(7, 214, 441);
        assert!(gs.hp_verified(), "a self OP_HPUpdate is the one reading that confirms the triple");

        // A client-derived write of the same fields — e.g. the bind-respawn full-HP assumption.
        gs.update_hp_estimated(7, 441, 441);
        assert_eq!(gs.cur_hp, 441, "control: the estimate path really does write the fields");
        assert!(!gs.hp_verified(), "one client-derived write is enough to spend the confirmation");

        // A second one. The counter is why nothing can hand trust back after accounting for only
        // one of several outstanding writes.
        gs.mark_hp_estimated();
        assert_eq!(gs.unverified_hp_writes, 2, "each client write is counted");
        assert!(!gs.hp_verified());

        // Only a real server reading restores trust — and it restores the value too.
        gs.update_hp(7, 214, 441);
        assert!(gs.hp_verified(), "the reconciling OP_HPUpdate is the sole path back to verified");
        assert_eq!(gs.cur_hp, 214, "and the server's figure replaces the estimate");
    }

    /// #1005 — reach control for the player branch: an HP update for SOMETHING ELSE must not
    /// confirm the player's own HP. Without this, any mob's OP_HPUpdate during a fight would clear
    /// the estimate debt and re-publish the client's inference as server truth.
    #[test]
    fn another_spawns_hp_update_does_not_confirm_the_players_own_hp_1005() {
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.update_hp(7, 214, 441);
        gs.update_hp_estimated(7, 107, 441);
        assert!(!gs.hp_verified(), "precondition: the player's HP is an estimate");

        gs.world.entities.insert(99, make_entity(99, "a rat", 0.0, 0.0, 0.0, true));
        gs.update_hp(99, 50, 100); // a mob we are fighting
        assert!(!gs.hp_verified(),
            "a mob's HP update says nothing about OUR HP — it must not clear the estimate debt");
        assert_eq!(gs.cur_hp, 107, "and it must not touch our HP either (control)");
    }

    /// #1005 — every observable derived from `cur_hp` must be covered, not just `hp` itself. With
    /// the player self-targeted (F1) `target_hp_pct` resolves from `self.hp_pct`, so it carries the
    /// same estimate; it must track it rather than sit stale beside a moved `hp_pct` (two different
    /// answers to "how much health do I have" in one payload), and `hp_verified` must govern it.
    #[test]
    fn self_target_hp_pct_follows_the_estimate_and_is_governed_by_hp_verified_1005() {
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.update_hp(7, 441, 441);
        gs.set_target(7); // F1
        assert_eq!(gs.target_hp_pct, Some(100.0));
        assert!(gs.hp_verified());

        gs.update_hp_estimated(7, 0, 441);
        assert_eq!(gs.cur_hp, 0);
        assert_eq!(gs.target_hp_pct, Some(0.0),
            "the self-target readout must not stay at 100 while hp_pct reads 0");
        assert!(!gs.hp_verified(),
            "target_hp_pct is self-HP while self-targeted, so the same flag has to cover it");
    }

    /// #1005 — the counter saturates rather than wrapping. A `u32` that wrapped to 0 would hand an
    /// unearned `hp_verified() == true` back to an agent after enough client-derived writes, which
    /// is the exact failure the flag exists to prevent. Cheap to assert, impossible to notice live.
    #[test]
    fn the_estimate_counter_saturates_and_never_wraps_back_to_verified_1005() {
        let mut gs = GameState::new();
        gs.player_id = 7;
        gs.update_hp(7, 441, 441);
        gs.unverified_hp_writes = u32::MAX;
        gs.mark_hp_estimated();
        assert_eq!(gs.unverified_hp_writes, u32::MAX, "saturating, not wrapping");
        assert!(!gs.hp_verified(), "a wrap here would publish client arithmetic as server truth");
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
        assert!(gs.world.entities.contains_key(&10));
        gs.remove_entity(10);
        assert!(!gs.world.entities.contains_key(&10));
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
        assert!(!gs.world.entities.contains_key(&1), "player must never appear in entities");
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
        assert_eq!(gs.world.entities.len(), 1);
        assert_eq!(gs.world.entities[&5].name, "updated");
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
        let e = &gs.world.entities[&7];
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
        let e = &gs.world.entities[&7];
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
    // `gs.world.entities` (register_spawn special-cases and skips the self-spawn).

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
        assert!(!gs.world.entities.contains_key(&1), "player must never appear in entities");
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
        assert!(gs.world.doors.get(&3).unwrap().is_open);
        gs.set_door_open(3, false);
        assert!(!gs.world.doors.get(&3).unwrap().is_open);
        // Unknown door id is ignored, not a panic.
        gs.set_door_open(99, true);
        assert!(!gs.world.doors.contains_key(&99));
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

    /// MVC C1 (#451): the WorldState boundary. Server-authoritative zone contents live in
    /// `gs.world`; the local-player PREDICTION (`player_x/y/z/heading`) is a separate slot that is
    /// NOT part of `world`. Purging the zone (a Model action) clears `world` but must leave the
    /// prediction untouched — proving the two are distinct storage, not the same conflated field.
    /// The "prediction is not in world" half is compile-enforced (there is no `world.player_x`);
    /// this test pins the runtime behavior of the split.
    #[test]
    fn worldstate_holds_zone_contents_and_is_separate_from_the_client_prediction() {
        let mut gs = GameState::new();
        // Client-prediction slot (owned by the render controller, mirrored here).
        gs.player_x = 111.0;
        gs.player_y = 222.0;
        gs.player_z = 333.0;
        gs.player_heading = 44.0;
        // Server-authoritative world (Model-written).
        gs.world.zone_name = "qeynos".into();
        gs.world.new_zone_applied = true;
        gs.upsert_entity(make_entity(7, "a rat", 0.0, 0.0, 0.0, true));
        gs.upsert_door(Door {
            door_id: 3, name: "DOOR1".into(), x: 0.0, y: 0.0, z: 0.0, heading: 0.0,
            incline: 0, size: 100, opentype: 5, door_param: 0, invert_state: false, is_open: false,
        });
        assert!(gs.world.entities.contains_key(&7) && gs.world.doors.contains_key(&3));

        // A zone purge (Model side) wipes the world contents...
        gs.begin_zone_in();
        assert!(gs.world.entities.is_empty(), "zone purge clears world entities");
        assert!(gs.world.doors.is_empty(), "zone purge clears world doors");
        assert!(!gs.world.new_zone_applied, "zone purge re-arms new_zone_applied");
        // ...but the client prediction is NOT world state, so it survives the purge untouched.
        assert_eq!([gs.player_x, gs.player_y, gs.player_z], [111.0, 222.0, 333.0],
            "predicted position is not WorldState — a zone purge must not touch it");
        assert_eq!(gs.player_heading, 44.0, "predicted heading is not WorldState");
    }

    /// #683 review (F2) — the previous zone's advertised zone points must NOT survive a zone-in.
    ///
    /// Left in place they persist through the OP_NewZone→OP_SendZonepoints window under the NEW
    /// zone_id, where they can satisfy the unresolved-cross gates with ANOTHER zone's adverts —
    /// the exact stale-window premise the review falsified. The honest state for that window is an
    /// EMPTY list until the new zone's own OP_SendZonepoints rebuilds it. Mutation check: drop the
    /// `zone_points.clear()` from `begin_zone_in` → RED.
    #[test]
    fn begin_zone_in_clears_the_previous_zones_zone_points_683() {
        let mut gs = GameState::new();
        gs.world.zone_points.push(ZonePoint {
            iterator: 7, zone_id: 181,
            server_x: 1790.0, server_y: 1315.0, server_z: -13.0, heading: 0.0,
        });

        gs.begin_zone_in();

        assert!(gs.world.zone_points.is_empty(),
            "the old zone's adverts are meaningless under the new zone_id — a zone-in must clear \
             them so the pre-OP_SendZonepoints window reads honestly empty");
    }

    /// #543/#660 B2 — the provisional-position marker must NOT survive a zone-in.
    ///
    /// A crossing marks the position provisional (the advertised guess). When that crossing was a
    /// real zone change, we zone into a new zone whose handshake brings an authoritative position,
    /// so the guess is moot. Verified LIVE that without this the marker stuck `true` in the new zone
    /// for 30s+ while `pos` was actually the correct zone-in point — a false marker, which is as
    /// much a lie as a missing one. The honest post-zone-in state is `player_pos_known = false`
    /// (unknown), not "provisional guess".
    #[test]
    fn begin_zone_in_clears_the_provisional_position_marker_660() {
        let mut gs = GameState::new();
        gs.position_provisional_since = Some(std::time::Instant::now());
        gs.player_pos_known = true;

        gs.begin_zone_in();

        assert!(gs.position_provisional_since.is_none(),
            "a zone-in makes the crossing guess moot — the marker must not stick true across it");
        assert!(!gs.player_pos_known,
            "…and the honest post-zone-in position state is UNKNOWN, not a provisional guess");
    }

    /// #724 round-3 review (B1) — the previous zone's hold must NOT survive a zone-in.
    ///
    /// The field has exactly two writers — this clear, and `ActionLoop::stream_position`'s mirror of
    /// `ControllerView::hold` — so while the zone-entry handshake runs, this clear is the only thing
    /// that can make the field honest. (#724's review also traced the net tick loop as *suspended*
    /// across the handshake, so the mirror cannot even race it; that is a reviewer's code trace of an
    /// async call graph, not measured, and this test does not depend on it.)
    ///
    /// MEASURED unpinned before this test existed: deleting the clear (with `app.rs`'s `clear_hold`
    /// call) left the whole workspace green, 158 passed / 0 failed. Mutation check: drop
    /// `player_hold = None` from `begin_zone_in` → RED here.
    #[test]
    fn begin_zone_in_clears_the_previous_zones_hold_724() {
        let mut gs = GameState::new();
        gs.player_hold = Some(ControllerHold {
            reason: ControllerHoldReason::EmbeddedNoRecovery,
            secs: 9.5,
        });

        gs.begin_zone_in();

        assert!(gs.player_hold.is_none(),
            "a hold describes geometry the zone-in just dropped, and nothing recomputes it while \
             the new zone loads — a zone-in must clear it so the load window reads honestly \
             \"no hold\" instead of a confident wedge alarm about the zone we left");
    }

    /// #884 — `ALL` is DERIVED, so it cannot go stale, and every variant answers `motion()`.
    ///
    /// The derivation is what is under test here, not the two rows: `controller_hold_reason!` emits
    /// the enum and `ALL` from one token stream, so a third variant appears in `ALL` on the next
    /// build without anyone remembering to add it, and `motion()`'s exhaustive `match` refuses to
    /// compile until its author says which [`HeldMotion`] it is. The `as_str` distinctness assert is
    /// the anti-inheritance half: a new variant that silently copies an existing token would make
    /// the machine-branchable name ambiguous, which is the same defect shape #890 measured.
    ///
    /// MUTATION-CHECK (both directions): drop a variant from the `ALL` list and the macro no longer
    /// compiles (the list IS the declaration). Wrap the body of `motion` so that it is neutered and
    /// always answers `HeldMotion::AllFrozen`, and the literal-valued anchor below —
    /// `hold_motion_matches_the_controllers_control_flow_884` — goes RED on the underworld row.
    #[test]
    fn controller_hold_reason_all_is_derived_and_total_884() {
        assert_eq!(ControllerHoldReason::ALL.len(), 2,
            "update this count deliberately when a hold reason is added — and check that #884's \
             HTTP suites still say the right thing about the new one: {:?}",
            ControllerHoldReason::ALL);
        let mut tokens: Vec<&str> = ControllerHoldReason::ALL.iter().map(|r| r.as_str()).collect();
        let n = tokens.len();
        tokens.sort_unstable();
        tokens.dedup();
        assert_eq!(tokens.len(), n,
            "two hold reasons share an `as_str` token — agents branch on these, so they must be \
             distinct: {tokens:?}");
        // Every variant answers; `motion()` is exhaustive, so this cannot panic — it exists to
        // prove the call is reachable for each of them from `ALL` alone.
        for r in ControllerHoldReason::ALL {
            let _: HeldMotion = r.motion();
        }
    }

    /// #884 — each hold reason's [`HeldMotion`] must match what `src/movement.rs` actually does with
    /// driver input on that arm. Read off the control flow, and re-checked at the source when this
    /// test is touched:
    ///
    /// * `EmbeddedNoRecovery` is raised inside `depenetrate`, and `step`'s first statement after
    ///   the hold-take is `if self.depenetrate(..) { … return self.pos; }` — the frame ends before
    ///   `intent` is read. `AllFrozen`.
    /// * `UnderworldNoRecovery` is raised by the #150 fall-through guard, which runs *after* the
    ///   collide-and-slide block has already written the frame's lateral wish into `self.pos`.
    ///   `LateralStillReaches`.
    ///
    /// This is the whole warrant for #884's HTTP gate refusing one and not the other, so it is
    /// asserted rather than left in prose. Refusing `/v1/move/manual` on the underworld arm would
    /// remove that body's ONLY client-API exit (`last_resort_placement`'s doc) and would itself be a
    /// false "you cannot move" — the mirror-image honesty bug.
    #[test]
    fn hold_motion_matches_the_controllers_control_flow_884() {
        assert_eq!(ControllerHoldReason::EmbeddedNoRecovery.motion(), HeldMotion::AllFrozen,
            "`depenetrate` returning true makes `step` return before it reads `intent`");
        assert_eq!(ControllerHoldReason::UnderworldNoRecovery.motion(), HeldMotion::LateralStillReaches,
            "the #150 fall-through guard runs after collide-and-slide has applied the lateral wish");
    }

    /// #801 — the previous zone's afloat stall must NOT survive a zone-in either.
    ///
    /// The sibling of `..._hold_724` above, and the case for it is *sharper*: an
    /// [`AfloatStall`](crate::afloat::AfloatStall) names an **anchor**, a specific
    /// `[east, north, up]` in the departed zone's coordinate frame, so carried across a crossing it
    /// is not a stale number but a confident falsehood with coordinates attached.
    ///
    /// **This clear alone does NOT cover "the render loop publishes nothing at all"** — measured: the
    /// mirror in `ActionLoop::stream_position` is unconditional, so it restored the departed zone's
    /// value on the next net tick and the clear survived about 10 ms. That case is covered because
    /// the net-thread zone-in path goes through `eqoxide_ipc::ControllerSlots::begin_zone_in`, which
    /// invalidates the `ControllerView` as well — see
    /// `a_zone_in_clears_the_departed_zones_hold_for_good_846` in `eqoxide-net`, which is the test
    /// that would have caught it (this one cannot: it never runs a mirror tick).
    ///
    /// The fixture uses a REAL matured stall from the real clock, not a hand-built value — the type
    /// has no public constructor, by design (see `crates/eqoxide-core/tests/afloat_unconstructible.rs`),
    /// so this is the only way to obtain one and the test could not fake it if it wanted to.
    ///
    /// Mutation check (#801, run independently): drop `self.player_afloat_stall = None;` from
    /// `begin_zone_in` → RED here.
    #[test]
    fn begin_zone_in_clears_the_previous_zones_afloat_stall_801() {
        use crate::afloat::{AfloatFrame, AfloatStallClock, AFLOAT_STALL_SECS};

        let mut gs = GameState::new();

        // Mature a genuine stall: a body held at one point under a sustained horizontal wish.
        let anchor = [-812.5_f32, 43.0, -119.75];
        let mut clock = AfloatStallClock::default();
        for _ in 0..((AFLOAT_STALL_SECS / 0.05).ceil() as usize + 3) {
            clock.observe(AfloatFrame::Wished, anchor, 0.05);
        }
        let stall = clock.stall().expect("fixture must actually reach the disclosure threshold");
        assert_eq!(stall.anchor(), anchor, "fixture sanity: the anchor is the departed zone's");
        gs.player_afloat_stall = Some(stall);

        gs.begin_zone_in();

        assert!(gs.player_afloat_stall.is_none(),
            "an afloat stall names an anchor in the zone we just left; nothing recomputes it while \
             the new zone loads, so a zone-in must clear it — otherwise the API keeps reporting a \
             trapped swimmer at coordinates that belong to a different zone");
    }

    /// #757 — `zone_cross_attempts` and `zone_cross_plan` must NOT survive a zone-in.
    ///
    /// Both fields are per-zone-namespace facts — see their doc comments. The fixture below is built
    /// to actually REACH the blocking / best-effort states, not merely a `Some(..)` that happens to
    /// clear.
    ///
    /// Before this test, dropping either clear still compiled and passed the whole suite;
    /// `player_hold`'s sibling clear was the one exception, because `..._hold_724` above already
    /// guarded it. Mutation check (each run independently, verbatim output in the #757 PR body):
    /// drop `self.zone_cross_attempts = None;` from `begin_zone_in` → RED at the first assertion;
    /// restore it, then drop `self.zone_cross_plan = None;` → RED at the second.
    #[test]
    fn begin_zone_in_clears_the_previous_zones_cross_attempts_and_plan_757() {
        use crate::zone_cross::{CrossAttempts, ZoneCrossPlan, ZoneCrossResolution, MAX_CROSS_ATTEMPTS};

        let mut gs = GameState::new();
        // Build a tally that has actually REACHED the bound (`blocks() == true`), not just any
        // `Some`. `CrossAttempts`'s own doc ("why the tally is NOT per region index") establishes
        // the count is a per-stand total, not keyed to whichever index was last attempted — the
        // predicate that matters here is `blocks()`, so the fixture drives it there directly rather
        // than asserting a claim about the type that its own docs disclaim.
        let mut tally = None;
        for index in 0..MAX_CROSS_ATTEMPTS as i32 {
            tally = Some(CrossAttempts::record(tally, index));
        }
        gs.zone_cross_attempts = tally;
        assert!(gs.zone_cross_attempts.unwrap().blocks(),
            "test setup: the fixture must reach the actual blocking bound, or this test does not \
             exercise the harm its assertion describes");
        gs.zone_cross_plan = Some(ZoneCrossPlan {
            requested_zone_id: Some(181),
            index: 3,
            resolution: ZoneCrossResolution::ServerResolved,
        });
        assert!(gs.zone_cross_plan.unwrap().resolution.is_best_effort(),
            "test setup: the fixture must be the best-effort case, since that is the only case \
             GET /v1/observe/debug discloses (`observe.rs`'s `.filter(|p| p.resolution.is_best_effort())`)");

        gs.begin_zone_in();

        assert!(gs.zone_cross_attempts.is_none(),
            "a cross-attempt tally counts attempts made during ONE continuous stand and blocks \
             further auto-cross once the count reaches MAX_CROSS_ATTEMPTS — carried into a new \
             zone, an already-blocked tally would leave the new zone's auto-cross gate stuck \
             refusing to retry for a limit the OLD zone's stand reached, not this one's; a \
             zone-in must clear it so the new zone starts unblocked");
        assert!(gs.zone_cross_plan.is_none(),
            "a zone-cross plan's index and requested zone id are only meaningful against the zone \
             that produced them, and a best-effort resolution left in place would keep \
             /v1/observe/debug reporting a degraded-destination crossing for a request the new \
             zone never made — a zone-in must clear it so that disclosure reads null until this \
             zone resolves a crossing of its own");
    }

    /// #883 — the previous zone's `last_consider` must NOT survive a zone-in.
    ///
    /// `last_consider` is spawn-scoped exactly like `target_id`/`target_con*`, but it is deliberately
    /// NOT touched by `clear_target()` (see that field's own doc), which is why the clear was missed:
    /// `clear_target()` was the only place anyone would have thought to add a sibling. Measured live
    /// on `main` @ `d63776d`, both directions (qcat↔qeynos2), three transitions — see the #883 issue
    /// body for the exact before/after JSON.
    ///
    /// Mutation check: drop `self.last_consider = None;` from `begin_zone_in` → RED here.
    #[test]
    fn begin_zone_in_clears_the_previous_zones_last_consider_883() {
        let mut gs = GameState::new();
        gs.last_consider = Some(LastConsider {
            spawn_id: 18,
            name: "a_large_rat000".into(),
            con_name: "gray".into(),
            attitude: "indifferent".into(),
            level: Some(1),
            at: std::time::Instant::now(),
        });

        gs.begin_zone_in();

        assert!(gs.last_consider.is_none(),
            "last_consider names a spawn_id in the zone we just left — spawn ids are a per-zone \
             namespace, so the same id in the new zone is ordinarily a different mob at a different \
             difficulty; a zone-in must clear it so /observe/debug reads null (\"nothing considered \
             in this zone yet\") instead of a confident, freshly-timestamped answer about a mob \
             that isn't here");
    }

    /// #883 review — exhaustive/combined variant of the individual clears above (#408/#660/#724/
    /// #801/#757/#883). Each of those pins ONE field in isolation; this populates EVERY field
    /// `begin_zone_in` currently owns — the complete documented clear-list, including
    /// `last_consider` — in a single `GameState`, calls `begin_zone_in()` exactly once, and asserts
    /// every one of them came back cleared. This is what actually backs the universal claim ("no
    /// spawn-scoped field this function owns survives a zone-in"): the per-field tests above would
    /// all still pass individually even if a future edit reordered the clears so an EARLIER one
    /// undid a LATER one's precondition (e.g. one clear rebuilding a value another clear reads) —
    /// this test is the one place that would catch that class of regression, because it is the only
    /// one exercising all of them together from one populated state.
    ///
    /// It does NOT claim coverage of every spawn-scoped field in `GameState` — only the ones
    /// `begin_zone_in` is documented (by the comments on each clear, above) to own. Until #941 that
    /// excluded `merchant_open`/`merchant_items`, `trainer_open`/`trainer_skills`, `pet_id`, the
    /// loot-session fields, `dialogue_choices`, `combat_anims` and `recent_attackers` — the broader
    /// audit the #883 PR body recorded. #941 gave `begin_zone_in` all of them, so they are populated
    /// and asserted here too (below the `── #941 ──` markers), and the exclusion note that used to
    /// stand here would now be a FALSE claim about what this function does. What is still not
    /// claimed is that no OTHER `GameState` field is zone-scoped; the field that guards THAT is
    /// `every_game_state_field_is_classified_against_begin_zone_in_941`, which is a compile-time
    /// guard, not an assertion.
    #[test]
    fn begin_zone_in_clears_every_field_it_owns_at_once_883() {
        use crate::zone_cross::{CrossAttempts, ZoneCrossPlan, ZoneCrossResolution, MAX_CROSS_ATTEMPTS};
        use crate::afloat::{AfloatFrame, AfloatStallClock, AFLOAT_STALL_SECS};

        let mut gs = GameState::new();

        gs.world.entities.insert(7, make_entity(7, "a rat", 0.0, 0.0, 0.0, true));
        gs.world.doors.insert(3, Door {
            door_id: 3, name: "DOOR1".into(), x: 0.0, y: 0.0, z: 0.0, heading: 0.0,
            incline: 0, size: 100, opentype: 5, door_param: 0, invert_state: false, is_open: false,
        });
        gs.player_pos_known = true;
        gs.position_provisional_since = Some(std::time::Instant::now());
        let mut tally = None;
        for index in 0..MAX_CROSS_ATTEMPTS as i32 {
            tally = Some(CrossAttempts::record(tally, index));
        }
        gs.zone_cross_attempts = tally;
        gs.zone_cross_plan = Some(ZoneCrossPlan {
            requested_zone_id: Some(181),
            index: 3,
            resolution: ZoneCrossResolution::ServerResolved,
        });
        gs.player_hold = Some(ControllerHold {
            reason: ControllerHoldReason::EmbeddedNoRecovery,
            secs: 9.5,
        });
        let anchor = [-812.5_f32, 43.0, -119.75];
        let mut clock = AfloatStallClock::default();
        for _ in 0..((AFLOAT_STALL_SECS / 0.05).ceil() as usize + 3) {
            clock.observe(AfloatFrame::Wished, anchor, 0.05);
        }
        gs.player_afloat_stall = Some(clock.stall().expect("fixture must reach the stall threshold"));
        gs.target_id = Some(18);
        gs.target_name = Some("Guard_Drath000".into());
        gs.target_hp_pct = Some(100.0);
        gs.target_con = Some([1, 2, 3]);
        gs.target_con_name = Some("yellow".into());
        gs.target_attitude = Some("indifferent".into());
        gs.last_consider = Some(LastConsider {
            spawn_id: 18,
            name: "a_large_rat000".into(),
            con_name: "gray".into(),
            attitude: "indifferent".into(),
            level: Some(1),
            at: std::time::Instant::now(),
        });
        gs.world.new_zone_applied = true;
        gs.world.zone_points.push(ZonePoint {
            iterator: 7, zone_id: 181,
            server_x: 1790.0, server_y: 1315.0, server_z: -13.0, heading: 0.0,
        });
        gs.world.zone_in_failed = true;
        gs.pending_cast_end = Some(std::time::Instant::now());
        gs.ended_cast_spell = Some((202, std::time::Instant::now()));
        gs.suppress_cast_end = true;
        gs.casting = Some(CastState { spell_id: 202, started: std::time::Instant::now(), cast_ms: 3000 });
        // ── #941 ──
        gs.dialogue_choices.push(saylink_choice("bind your soul"));
        gs.trainer_open = Some(41);
        gs.trainer_skills = vec![0; 78];
        gs.trainer_skills[1] = 200;
        gs.merchant_open = Some(111);
        gs.merchant_items.push(MerchantItem {
            merchant_slot: 1, item_id: 1, name: "Rusty Dagger".into(), icon: 0, price: 5, quantity: 1,
        });
        gs.task_offers.push(TaskOffer {
            task_id: 10, npc_id: 9001, title: "Offer One".into(),
            description: "…".into(), has_rewards: true,
        });
        gs.pet_id = Some(64);
        populate_loot_session(&mut gs);
        gs.combat_anims.insert(7, (5, std::time::Instant::now()));
        gs.recent_attackers.insert(7, std::time::Instant::now());

        gs.begin_zone_in();

        assert!(gs.world.entities.is_empty(), "entities");
        assert!(gs.world.doors.is_empty(), "doors");
        assert!(!gs.player_pos_known, "player_pos_known");
        assert!(gs.position_provisional_since.is_none(), "position_provisional_since");
        assert!(gs.zone_cross_attempts.is_none(), "zone_cross_attempts");
        assert!(gs.zone_cross_plan.is_none(), "zone_cross_plan");
        assert!(gs.player_hold.is_none(), "player_hold");
        assert!(gs.player_afloat_stall.is_none(), "player_afloat_stall");
        assert!(gs.target_id.is_none(), "target_id");
        assert!(gs.target_name.is_none(), "target_name");
        assert!(gs.target_hp_pct.is_none(), "target_hp_pct");
        assert!(gs.target_con.is_none(), "target_con");
        assert!(gs.target_con_name.is_none(), "target_con_name");
        assert!(gs.target_attitude.is_none(), "target_attitude");
        assert!(gs.last_consider.is_none(), "last_consider");
        assert!(!gs.world.new_zone_applied, "new_zone_applied");
        assert!(gs.world.zone_points.is_empty(), "zone_points");
        assert!(!gs.world.zone_in_failed, "zone_in_failed");
        assert!(gs.pending_cast_end.is_none(), "pending_cast_end");
        assert!(gs.ended_cast_spell.is_none(), "ended_cast_spell");
        assert!(!gs.suppress_cast_end, "suppress_cast_end");
        assert!(gs.casting.is_none(), "casting");
        // ── #941 ──
        assert!(gs.dialogue_choices.is_empty(), "dialogue_choices");
        assert!(gs.trainer_open.is_none(), "trainer_open");
        assert!(gs.trainer_skills.is_empty(), "trainer_skills");
        assert!(gs.merchant_open.is_none(), "merchant_open");
        assert!(gs.merchant_items.is_empty(), "merchant_items");
        assert!(gs.task_offers.is_empty(), "task_offers");
        assert!(gs.pet_id.is_none(), "pet_id");
        assert!(gs.pending_loot.is_empty(), "pending_loot");
        assert!(!gs.loot_session_active, "loot_session_active");
        assert!(!gs.loot_confirmed, "loot_confirmed");
        assert!(gs.loot_current_corpse.is_none(), "loot_current_corpse");
        assert!(gs.loot_last_activity.is_none(), "loot_last_activity");
        assert!(gs.loot_end_requested_at.is_none(), "loot_end_requested_at");
        assert!(gs.loot_queued_at.is_none(), "loot_queued_at");
        assert!(gs.loot_defensive_close_at.is_none(), "loot_defensive_close_at");
        assert!(gs.combat_anims.is_empty(), "combat_anims");
        assert!(gs.recent_attackers.is_empty(), "recent_attackers");
    }

    /// A saylink choice shaped like the ones `parse_say_links` builds from a real NPC line — the
    /// SAYLINK_ITEM_ID marker and a non-zero `augments[0]` sayid, which is what
    /// `POST /v1/interact/dialogue` actually sends back to the server. Built as a fixture helper so
    /// the #941 tests below (and the combined clear-list test above) all exercise a choice that
    /// could really be clicked, not an empty struct that happens to clear.
    fn saylink_choice(text: &str) -> DialogueChoice {
        DialogueChoice {
            text: text.to_string(),
            // 0xF_FFFF is `SAYLINK_ITEM_ID` — the marker the server requires on a saylink click.
            // Written as a literal because that const is private to `eq_net::packet_handler`,
            // which sits ABOVE this crate and cannot be imported from here.
            item_id: 0xF_FFFF,
            augments: [5, 0, 0, 0, 0, 0],
            link_hash: 0,
            icon: 0,
        }
    }

    /// Drive `gs` into a loot session that has actually REACHED the #414 defensive-close quarantine
    /// — the state in which `loot_tick_action` withholds the next corpse's `OP_LootRequest` — with a
    /// second corpse still queued behind it. A bare `Some(id)` would leave the assertions below
    /// unfalsifiable about the thing that matters: it is the *quarantined, non-empty-queue* session
    /// that a zone-in must not carry, because that is the one that both misdirects the drain (the
    /// queued id) and blocks the new zone's first loot (the quarantine).
    fn populate_loot_session(gs: &mut GameState) {
        let now = std::time::Instant::now();
        gs.pending_loot.push_back(31); // a second corpse, still queued, in the DEPARTED zone
        gs.loot_session_active = true;
        gs.loot_confirmed = true;
        gs.loot_current_corpse = Some(30);
        gs.loot_last_activity = Some(now);
        gs.loot_end_requested_at = Some(now);
        gs.loot_queued_at = Some(now);
        gs.loot_defensive_close_at = Some(now);
    }

    /// #941 (1/6) — the departed NPC's dialogue choices must NOT survive a zone-in.
    ///
    /// The highest-exposure field in the #941 audit and the one that matches this project's
    /// agent-honesty invariant most exactly: `GET /v1/observe/dialogue` serves these verbatim and
    /// `POST /v1/interact/dialogue` ACTS on them (it sends an OP_ItemLinkClick carrying the saylink's
    /// sayid). Carried across a zone line the endpoint answers with a well-formed, plausible, FALSE
    /// list — choices offered by an NPC that is not in this zone — and the click endpoint will
    /// happily fire one at the new zone's server.
    ///
    /// CODE-TRACED, not live-measured (the #941 issue asks for this to be said plainly): the field's
    /// only other writers are `apply_channel_message`/`apply_special_mesg` in `packet_handler`
    /// ("a new NPC line carried saylinks", overwrite-only, never clear) and `drain_chat`'s hail clear
    /// (#274). Neither runs on a zone-in, so before this fix the list survived until the agent
    /// happened to hail someone or another NPC happened to speak with links.
    ///
    /// Mutation check (both directions, verbatim output in the PR body): drop
    /// `self.dialogue_choices.clear();` → RED; and WRAP it as
    /// `if false { self.dialogue_choices.clear(); } else { self.dialogue_choices.truncate(1); }`
    /// (a different, plausible-looking edit that leaves the one stale choice in place) → also RED.
    #[test]
    fn begin_zone_in_clears_the_departed_npcs_dialogue_choices_941() {
        let mut gs = GameState::new();
        gs.dialogue_choices.push(saylink_choice("bind your soul"));
        gs.dialogue_choices.push(saylink_choice("train me"));
        assert_eq!(gs.dialogue_choices[0].augments[0], 5,
            "test setup: the fixture must be a CLICKABLE choice (a real sayid), or this test does \
             not exercise the harm — an un-clickable choice would only be a bad read, not a bad act");

        gs.begin_zone_in();

        assert!(gs.dialogue_choices.is_empty(),
            "dialogue_choices are the saylinks of ONE NPC's line in the zone we just left; carried \
             across a zone-in, GET /v1/observe/dialogue serves them as this zone's current choices \
             and POST /v1/interact/dialogue will send one — a confident falsehood the agent can \
             then act on, which is exactly the failure mode this client must never produce");
    }

    /// #1004 review — `task_offers` is the same shape as `dialogue_choices` above and belongs to
    /// the same class: each offer names the OFFERING NPC's spawn id (`npc_id`, per `TaskOffer`'s
    /// own doc — "required by `OP_AcceptNewTask`'s `task_master_id` field"), it is served verbatim
    /// by GET /v1/quests/offers, and it is directly ACTIONABLE — POST /v1/quests/accept resolves
    /// `task_master_id` from this list and sends OP_AcceptNewTask to it. A surviving offer after a
    /// zone-in is therefore a well-formed, plausible, false answer the agent can act on, against an
    /// NPC that is not in this zone — exactly `dialogue_choices`'s failure mode.
    ///
    /// Mutation check (both directions, verbatim output in the PR body): drop
    /// `self.task_offers.clear();` → RED; and WRAP it as
    /// `if std::hint::black_box(false) { self.task_offers.clear(); } else { self.task_offers.truncate(1); }`
    /// (a plausible "trim it down" edit that leaves one stale offer in place) → also RED.
    #[test]
    fn begin_zone_in_clears_the_departed_zones_task_offers_941() {
        let mut gs = GameState::new();
        gs.task_offers.push(TaskOffer {
            task_id: 10, npc_id: 9001, title: "Offer One".into(),
            description: "Slay the rats".into(), has_rewards: true,
        });
        gs.task_offers.push(TaskOffer {
            task_id: 11, npc_id: 9002, title: "Offer Two".into(),
            description: "Fetch the ore".into(), has_rewards: false,
        });

        gs.begin_zone_in();

        assert!(gs.task_offers.is_empty(),
            "task_offers name the OFFERING NPC's spawn id (npc_id); carried across a zone-in, \
             GET /v1/quests/offers serves them as this zone's current offers and POST \
             /v1/quests/accept will address OP_AcceptNewTask to a departed zone's spawn id — the \
             same confident-falsehood shape as dialogue_choices, and this client must never \
             produce it");
    }

    /// #941 (2/6) — the departed zone's trainer window must NOT survive a zone-in.
    ///
    /// `trainer_open` is a guildmaster's SPAWN id and `trainer_skills` are the caps THAT trainer
    /// offers. Both are exposed on `GET /v1/observe/debug` (`player.trainer_open` as a bool,
    /// `player.trainer_skills` as the cap list, `eqoxide-http/src/lib.rs`), and
    /// `POST /v1/trainer/train` gates on the bool and then addresses OP_GMTrainSkill to the id
    /// (`action_loop::drain_trainer`). A trainer window cannot survive a zone line — the NPC is not
    /// here — so a surviving id aims a training request at whatever spawn the NEW zone assigned that
    /// number, and a surviving cap list advertises skills this zone's trainer may not offer at all.
    ///
    /// CODE-TRACED: the only clear is the explicit `POST /v1/trainer/open {"trainer":0}` end-session
    /// sentinel (`drain_trainer`), which a zone change obviously does not send.
    ///
    /// Mutation check (both directions): drop the two clears → RED; WRAP as
    /// `if false { …clears… } else { self.trainer_skills.truncate(1); }` → also RED.
    #[test]
    fn begin_zone_in_clears_the_departed_zones_trainer_window_941() {
        let mut gs = GameState::new();
        gs.trainer_open = Some(41);
        gs.trainer_skills = vec![0; 78];
        gs.trainer_skills[1] = 200; // a cap this trainer offers — the shape /v1/train reads

        gs.begin_zone_in();

        assert!(gs.trainer_open.is_none(),
            "the open trainer's spawn id belongs to the zone we left; left set, /observe/debug \
             reports a training window as open here and POST /v1/trainer/train sends \
             OP_GMTrainSkill at that id — a spawn the new zone assigned to something else");
        assert!(gs.trainer_skills.is_empty(),
            "…and its cap list must go with it: caps are what THAT trainer offered, and an agent \
             reading them here would plan training this zone's trainer may not offer");
    }

    /// #941 (3/6) — the departed zone's merchant window must NOT survive a zone-in, AND the clear
    /// must compose with the existing no-flicker invariant rather than fight it.
    ///
    /// The #941 issue flags this field specifically because `begin_shop_open_for` already carries a
    /// deliberate, tested invariant (#361 review FIX 2): a pre-buy/pre-sell OP_ShopRequest RESEND
    /// against the merchant that is ALREADY open must NOT flicker `merchant_open` to `None` for a
    /// round-trip, because `sync_merchant` mirrors the field into `GET /v1/merchant/list` every tick
    /// and the HUD gates its window on `is_some()`. The two are orthogonal, and this test measures
    /// that rather than asserting it: after a zone-in it drives the full open → resend cycle and
    /// checks the resend still does not flicker. (The invariant's own tests,
    /// `shop_open_for_a_different_merchant_clears_the_previous_session_360` and
    /// `shop_open_resend_for_the_same_merchant_does_not_flicker_361`, are untouched and still pass —
    /// this clear does not go near `begin_shop_open_for`.)
    ///
    /// Mutation check (both directions): drop the two clears → RED at the first assertion; WRAP as
    /// `if false { …clears… } else { self.merchant_items.clear(); }` — i.e. the plausible
    /// half-fix that drops the wares but keeps the window "open" — → RED at the first assertion,
    /// with the second and third still passing, which is why all three are here.
    #[test]
    fn begin_zone_in_clears_the_departed_zones_merchant_window_941() {
        let mut gs = GameState::new();
        gs.merchant_open = Some(111);
        gs.merchant_items.push(MerchantItem {
            merchant_slot: 1, item_id: 1, name: "Rusty Dagger".into(), icon: 0, price: 5, quantity: 1,
        });

        gs.begin_zone_in();

        assert_eq!(gs.merchant_open, None,
            "an open merchant window cannot survive a zone line — the NPC is not here; left set, \
             GET /v1/merchant/list reports a shop open in this zone and a buy is addressed to a \
             spawn id the new zone gave to something else");
        assert!(gs.merchant_items.is_empty(),
            "…and its wares list is the DEPARTED merchant's inventory, item ids, prices and slots");

        // COMPOSITION with the #361 no-flicker guard: a first open in the new zone takes the guard's
        // clear-and-reopen path (`None != Some(222)`), and the routine resend against that
        // now-open merchant must still not flicker it closed.
        gs.begin_shop_open_for(222);
        gs.merchant_open = Some(222); // the server's OP_ShopRequest echo
        gs.merchant_items.push(MerchantItem {
            merchant_slot: 1, item_id: 2, name: "Cloth Cap".into(), icon: 0, price: 3, quantity: 1,
        });
        gs.begin_shop_open_for(222); // the pre-buy resend
        assert_eq!(gs.merchant_open, Some(222),
            "the zone-in clear must not disturb the #361 no-flicker invariant: a resend against the \
             merchant already open in the NEW zone must still not blink the window closed");
        assert!(!gs.merchant_items.is_empty(), "…nor drop its wares for a round-trip");
    }

    /// #941 (4/6) — the departed zone's `pet_id` must NOT survive a zone-in.
    ///
    /// A pet follows its owner across a zone line in EQ, but the NEW zone assigns it a NEW spawn
    /// record, so the id held here is stale from the instant we zone. `action_loop::drain_pet` uses
    /// it as the "do you have a pet" gate for `POST /v1/pet/command` and
    /// `drive_auto_pet_combat` reads it every tick; until the new zone's pet spawn packet rewrites
    /// it, an OP_PetCommands goes out addressed to a spawn that is not our pet.
    ///
    /// CODE-TRACED, and this one is a *structural* miss worth naming: `pet_id` IS cleared when the
    /// pet leaves — but only via `remove_entity`, which is the one path `begin_zone_in` cannot go
    /// through, because it empties `world.entities` wholesale rather than per-spawn. That is the
    /// same shape as #883's `last_consider` (cleared by nothing `clear_target` calls).
    ///
    /// Mutation check (both directions): drop `self.pet_id = None;` → RED; WRAP as
    /// `if false { self.pet_id = None; } else { self.world.entities.clear(); }` — the "surely the
    /// entity purge covers it" assumption stated as code — → also RED, which is the point.
    #[test]
    fn begin_zone_in_clears_the_departed_zones_pet_id_941() {
        let mut gs = GameState::new();
        gs.pet_id = Some(64);
        gs.world.entities.insert(64, make_entity(64, "Gynok`s pet", 0.0, 0.0, 0.0, true));

        gs.begin_zone_in();

        assert!(gs.pet_id.is_none(),
            "a pet's spawn id is per-zone: the pet follows us but the new zone re-registers it under \
             a new id, so this one now names whatever that zone assigned the number to. Note the \
             entity purge above does NOT cover this — `remove_entity` is what normally clears \
             `pet_id`, and `begin_zone_in` empties the map without going through it");
    }

    /// #941 (5/6) — the departed zone's loot session and queue must NOT survive a zone-in.
    ///
    /// Every id in `pending_loot` and in `loot_current_corpse` is a CORPSE spawn id in the zone we
    /// left. This is not HTTP-exposed (there is no `/v1/loot/status`), so the harm is internal
    /// rather than a direct API lie: the net thread's own auto-loot drain (`loot_tick_action` in
    /// `gameplay.rs`) would send this zone's first `OP_LootRequest` for a departed zone's corpse id,
    /// and a carried-over #414 defensive-close quarantine would instead WITHHOLD it until the
    /// quarantine timed out.
    ///
    /// ## On #414's "deliberately left as-is"
    /// `gameplay.rs`'s `LootTickAction::TimedOut` arm and `apply_loot_open_timeout` both deliberately
    /// leave `loot_session_active`/`loot_current_corpse` set. That note was read before this clear
    /// was written and it does NOT govern here: it keeps the session pinned so a LATE
    /// `OP_MoneyOnCorpse`/`OP_LootComplete` — neither carries a corpse id — cannot be misattributed
    /// to the next corpse's session. A zone change ends the zone-server session that could deliver
    /// such an ack (`run_gameplay_phase` drops the stream and `EqStream::connect`s a new one before
    /// the handshake that calls `begin_zone_in`), so there is nothing left to quarantine against.
    /// That is a code trace of the reconnect path, not a live measurement, and it is stated as one.
    /// This clear is scoped to `begin_zone_in`; the #414 arms themselves are untouched and their
    /// tests (`loot_close_timeout_keeps_the_session_pinned_414` and siblings in `gameplay.rs`) still
    /// pass.
    ///
    /// Mutation check (both directions): drop the eight clears → RED; WRAP them as
    /// `if false { …clears… } else { self.pending_loot.clear(); }` — the half-fix that drops the
    /// queue but leaves the quarantine armed — → also RED, at the `loot_session_active` assertion.
    #[test]
    fn begin_zone_in_clears_the_departed_zones_loot_session_941() {
        let mut gs = GameState::new();
        populate_loot_session(&mut gs);

        gs.begin_zone_in();

        assert!(gs.pending_loot.is_empty(),
            "a queued corpse id names a corpse in the zone we left; the auto-loot drain would send \
             this zone's OP_LootRequest for it, hitting whatever spawn holds that id here");
        assert!(!gs.loot_session_active,
            "no loot session can survive a zone line — and left true alongside the #414 \
             defensive-close quarantine below, it also WITHHOLDS the new zone's first loot until \
             that quarantine times out");
        assert!(!gs.loot_confirmed, "loot_confirmed says the DEPARTED corpse actually opened");
        assert_eq!(gs.loot_current_corpse, None,
            "the open corpse's spawn id is what OP_EndLootRequest is addressed to");
        assert!(gs.loot_last_activity.is_none(), "…and its inactivity clock");
        assert!(gs.loot_end_requested_at.is_none(), "…and its close-ack deadline");
        assert!(gs.loot_queued_at.is_none(), "…and the queue's open delay");
        assert!(gs.loot_defensive_close_at.is_none(),
            "…and the #414 quarantine itself: its whole premise is a late ack still arriving on \
             THIS zone-server session, which the zone change has ended");
    }

    /// #941 (6/6) — the departed zone's spawn-id-keyed combat maps must NOT survive a zone-in.
    ///
    /// Both are keyed by spawn id and neither is HTTP-exposed, so this is the lowest-exposure item
    /// in the #941 audit and is reported as such. It is still real:
    ///
    /// * `combat_anims` also keys the PLAYER's own id (`app.rs` reads `combat_anims[&player_id]`),
    ///   and the player's id is the one id that does NOT change across a zone line — so a surviving
    ///   entry replays the departed zone's swing on the first frames of the new zone with no packet
    ///   behind it.
    /// * `recent_attackers` feeds the auto-combat add-retarget. Its window is bounded (a 6s
    ///   `ATTACKER_TTL` prune in `action_loop`) and its consumer additionally requires the id to
    ///   resolve to a live, reachable NPC — so the surviving harm is narrow: an id the new zone
    ///   reuses inside that window being treated as something that just attacked us. Narrow is a
    ///   bound on a wrong answer, not a reason to keep it.
    ///
    /// Mutation check (both directions): drop the two clears → RED; WRAP as
    /// `if false { …clears… } else { self.combat_anims.clear(); }` → RED at the
    /// `recent_attackers` assertion.
    #[test]
    fn begin_zone_in_clears_the_departed_zones_spawn_keyed_combat_maps_941() {
        let mut gs = GameState::new();
        gs.player_id = 12;
        gs.combat_anims.insert(12, (5, std::time::Instant::now())); // the PLAYER's own mid-swing
        gs.combat_anims.insert(7, (5, std::time::Instant::now()));  // …and a mob's
        gs.recent_attackers.insert(7, std::time::Instant::now());

        gs.begin_zone_in();

        assert!(gs.combat_anims.is_empty(),
            "a swing is an event in the zone we left; the player's own id survives the zone line, \
             so a surviving entry replays that swing in the new zone with no packet behind it");
        assert!(gs.recent_attackers.is_empty(),
            "an attacker id is a per-zone spawn id; inside the 6s TTL, an id the new zone reuses \
             would be treated by the auto-combat retarget as something that just attacked us");
    }

    /// #941 — the CLASS guard. Not an assertion test: a COMPILE-TIME forcing function.
    ///
    /// #941's own framing is that "spawn-scoped" is a property no type enforces, so `begin_zone_in`
    /// is a hand-maintained clear-list that has now been found incomplete three times (#757, #883,
    /// and the eight fields #941 itself found). The strongest available fix — moving every
    /// zone-scoped field behind one sub-struct that `begin_zone_in` resets wholesale, making
    /// "forgotten" unrepresentable — is NOT what this test is, and the PR that added it says so:
    /// those fields are read from ~180 call sites across eight crates (including `src/app.rs`'s
    /// `combat_anims` read), which is a refactor with its own blast radius and its own review, and
    /// doing it for only these eight would leave `begin_zone_in` owning BOTH a sub-struct and a hand
    /// list — closing nothing.
    ///
    /// What this DOES close is the step before that one: a new `GameState` field can no longer be
    /// added without someone deciding which group it belongs to. The destructure below is exhaustive
    /// and deliberately has NO `..`, so adding a field to `GameState` makes this file FAIL TO
    /// COMPILE until the field is listed under one of the two headings. It cannot check that the
    /// classification is *correct* — only that it was made, deliberately, at the moment the field
    /// was introduced, which is precisely the moment the three misses above were available to catch.
    /// The correctness half stays with `begin_zone_in_clears_every_field_it_owns_at_once_883`, which
    /// asserts that everything in the first group actually comes back cleared.
    ///
    /// Mutation check: add any field to `GameState` without touching this list → this file fails to
    /// compile, naming the missing field. (Measured, both directions — see the PR body.)
    #[test]
    fn every_game_state_field_is_classified_against_begin_zone_in_941() {
        let GameState {
            // ── ZONE-SCOPED: cleared by `begin_zone_in`. Asserted by the combined test above. ──
            // (`world` is listed here for its zone-scoped contents — entities, doors, zone_points,
            // new_zone_applied, zone_in_failed. `world.zone_name` is deliberately NOT cleared; see
            // `begin_zone_in`'s comment on `zone_in_failed`.)
            world: _,
            player_pos_known: _, position_provisional_since: _,
            zone_cross_attempts: _, zone_cross_plan: _,
            player_hold: _, player_afloat_stall: _,
            target_id: _, target_name: _, target_hp_pct: _,
            target_con: _, target_con_name: _, target_attitude: _,
            last_consider: _,
            casting: _, pending_cast_end: _, ended_cast_spell: _, suppress_cast_end: _,
            dialogue_choices: _,
            trainer_open: _, trainer_skills: _,
            merchant_open: _, merchant_items: _,
            pet_id: _,
            pending_loot: _, loot_session_active: _, loot_confirmed: _, loot_current_corpse: _,
            loot_last_activity: _, loot_end_requested_at: _, loot_queued_at: _,
            loot_defensive_close_at: _,
            combat_anims: _, recent_attackers: _,
            task_offers: _,

            // ── NOT ZONE-SCOPED: survives a zone line by design. ──
            // Identity + character sheet: the same character, in a different zone.
            player_id: _, player_name: _, player_level: _, player_race: _, player_class: _,
            player_gender: _, player_face: _, player_hairstyle: _, player_haircolor: _,
            player_guild_id: _, player_guild_rank: _, stats: _, player_skills: _,
            player_equipment: _, player_equipment_tint: _, mem_spells: _,
            // Position/posture: `player_x/y/z` deliberately keep the last-known numbers (there is
            // nothing else to set them to) — `player_pos_known`, above, is what marks them untrusted.
            player_x: _, player_y: _, player_z: _, player_heading: _, player_action: _,
            sitting: _, run_mode: _, auto_attack: _, levitate: _,
            // Vitals + wallet: server truth about the player, not about a spawn.
            // (#1005: `hp_confirmed`/`unverified_hp_writes` are the HP counterparts of
            // `coin_confirmed`/`unverified_buys` on the next line, and are classified the same way.
            // They survive a zone line because `cur_hp`/`max_hp` do — clearing the debt while
            // keeping the numbers it describes would republish the same values as confirmed. The
            // fresh PlayerProfile every zone-in delivers re-marks them as an estimate regardless,
            // since the profile carries no max.)
            hp_pct: _, cur_hp: _, max_hp: _, hp_confirmed: _, unverified_hp_writes: _,
            mana_pct: _, cur_mana: _, max_mana: _, xp_pct: _,
            coin: _, coin_confirmed: _, unverified_buys: _,
            // Death record: `last_cast`-shaped — a true record of something that already happened.
            player_dead: _, player_dead_since: _, killed_by: _, died_at: _, last_cast: _,
            // Group/guild/social: keyed by NAME, and a group follows the player across a zone line.
            group_members: _, group_leader: _, pending_invite: _,
            guild_names: _, guild_members: _, pending_guild_invite: _, who_roster: _,
            // Inventory + quest log: character-scoped, server-replicated, not spawn-keyed.
            // (`task_offers` is NOT here — see the ZONE-SCOPED group above: each offer names the
            // offering NPC's spawn id, `npc_id`, per its own doc, so it is spawn-keyed the same way
            // `dialogue_choices` is. `tasks`/`completed_task_history` are the accepted/finished
            // task log — no NPC id, genuinely character-scoped.)
            inventory: _, inventory_received: _, trade_ack_ready: _,
            tasks: _, completed_task_history: _,
            // Logs, feeds and session plumbing.
            messages: _, chat_events: _, next_chat_id: _, last_book_text: _, ucs: _,
            strategy: _, server_corrections: _,
        } = GameState::new();
    }

}
