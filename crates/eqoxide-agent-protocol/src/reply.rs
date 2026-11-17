//! The body each op answers with, as one type per op.
//!
//! A [`CallReply`](crate::call::CallReply) carries its body as JSON because the reply does not name
//! the op that produced it. [`Reply::decode`] reads that body back into the type of the `Op` the
//! caller sent, and [`Reply`] writes a typed body as the same JSON.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::op::Op;

/// What a tracked op answers with once the host has admitted it.
///
/// `Accepted` means the host queued the action and the game has not confirmed it. `Unconfirmed`
/// means the host waited for a confirmation that did not arrive. `Open`, `Bought` and `Given` are
/// confirmations the server echoed. The host fills `request_id` with the id that the action event
/// feed reports for the same action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Receipt {
    Accepted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        message: String,
    },
    Unconfirmed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        message: String,
        /// Merchant that the call named, for `merchant.open` and `merchant.buy`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        merchant: Option<String>,
        /// Merchant slot that the call named, for `merchant.buy`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        slot: Option<u32>,
        /// NPC that the call named, for `interact.give`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        npc: Option<String>,
        /// Inventory slot that the call named, for `interact.give`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        from: Option<u32>,
    },
    Open {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        merchant_id: u32,
    },
    Bought {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        item: String,
        price: u32,
        coin_after: Currency,
    },
    Given {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<u64>,
        item: String,
        npc_id: u32,
    },
}

impl Receipt {
    /// Whether the server confirmed the action.
    pub fn is_confirmed(&self) -> bool {
        matches!(self, Self::Open { .. } | Self::Bought { .. } | Self::Given { .. })
    }

    /// The text that the action event feed reports as the reason for the outcome.
    pub fn reason(&self) -> &str {
        match self {
            Self::Accepted { message, .. } | Self::Unconfirmed { message, .. } => message,
            Self::Open { .. } => "open",
            Self::Bought { .. } => "bought",
            Self::Given { .. } => "given",
        }
    }

    /// Sets the action id that the event feed reports for this action.
    pub fn with_request_id(mut self, id: u64) -> Self {
        match &mut self {
            Self::Accepted { request_id, .. }
            | Self::Unconfirmed { request_id, .. }
            | Self::Open { request_id, .. }
            | Self::Bought { request_id, .. }
            | Self::Given { request_id, .. } => *request_id = Some(id),
        }
        self
    }
}

/// A coin balance split by denomination.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Currency {
    pub platinum: u32,
    pub gold: u32,
    pub silver: u32,
    pub copper: u32,
}

/// `group.roster`. An empty `members` means the player is not grouped.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupRoster {
    pub leader: String,
    pub pending_invite: Option<String>,
    pub you_are_leader: bool,
    pub members: Vec<GroupMember>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupMember {
    pub name: String,
    pub level: u32,
    pub is_leader: bool,
    pub is_merc: bool,
    pub tank: bool,
    pub assist: bool,
    pub puller: bool,
    pub offline: bool,
    pub hp_pct: f32,
}

/// `guild.roster`. A `guild_id` of 0 and an empty `members` mean the player is not in a guild.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildRoster {
    pub guild: String,
    pub guild_id: u32,
    pub guild_rank: u32,
    pub pending_invite: Option<String>,
    pub members: Vec<GuildMember>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GuildMember {
    pub name: String,
    pub rank: u32,
    pub level: u32,
    pub class: u32,
    /// Zone the member was last seen in, 0 when offline.
    pub zone_id: u32,
    pub online: bool,
    pub public_note: String,
}

/// `merchant.list`. `open` is false when no merchant window is open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MerchantList {
    pub open: bool,
    pub merchant_id: Option<u32>,
    pub count: usize,
    pub items: Vec<MerchantItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MerchantItem {
    pub merchant_slot: u32,
    pub item_id: u32,
    pub name: String,
    pub icon: u32,
    pub price: u32,
    /// Stock count, -1 or a large value for unlimited.
    pub quantity: i32,
}

/// `quests.log`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestLog {
    pub active_count: usize,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub task_id: u32,
    pub title: String,
    pub description: String,
    pub xp_reward: u32,
    pub coin_reward: u32,
    /// Reward item name, empty when the task has none.
    pub reward_item_text: String,
    pub status: TaskStatus,
    /// Journal slot that the server addresses a task by when it cancels the task.
    pub sequence_number: u32,
    pub activities: Vec<TaskActivity>,
}

/// One objective of a [`Task`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskActivity {
    pub activity_id: u32,
    #[serde(flatten)]
    pub progress: ActivityProgress,
}

/// What the server disclosed about an objective. `Locked` objectives carry no counts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ActivityProgress {
    Known {
        activity_type: i32,
        target: String,
        description: String,
        done_count: u32,
        goal_count: u32,
        optional: bool,
    },
    Locked {
        optional: bool,
    },
    Undecodable {
        reason: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Active,
    Completed,
    Cancelled,
}

/// `quests.completed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestsCompleted {
    pub count: usize,
    pub completed: Vec<CompletedTask>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedTask {
    pub task_id: u32,
    pub title: String,
    /// Unix time the server reports for the completion.
    pub completed_time: u32,
}

/// `quests.offers`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuestOffers {
    pub count: usize,
    pub offers: Vec<TaskOffer>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskOffer {
    pub task_id: u32,
    pub npc_id: u32,
    pub title: String,
    pub description: String,
    pub has_rewards: bool,
}

/// `social.friends`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Friends {
    pub friends: Vec<Friend>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Friend {
    pub name: String,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub class: Option<String>,
}

/// `trainer.list`. `open` is false when no trainer window is open.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainerList {
    pub open: bool,
    pub skills: Vec<TrainableSkill>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrainableSkill {
    pub id: usize,
    pub name: Option<String>,
    pub current: u32,
    pub cap: u32,
}

/// `events.read`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Events {
    pub count: usize,
    pub last_id: u64,
    pub first_id: u64,
    /// Events lost to ring eviction between `since` and `first_id`.
    pub dropped: u64,
    pub category: Option<crate::op::EventCategory>,
    pub events: Vec<Event>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    pub id: u64,
    pub category: String,
    pub kind: String,
    pub from: String,
    pub directed: bool,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// `observe.item_text`. `text` is `None` until a book or note has been read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemText {
    pub text: Option<String>,
    pub snapshot_age_ms: u64,
}

/// `observe.who`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Who {
    pub online: Vec<WhoEntry>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WhoEntry {
    pub name: String,
    pub level: u32,
    /// Class name, empty when the player is anonymous.
    pub class: String,
    /// Race code, empty when the player is anonymous.
    pub race: String,
    /// Zone the player is in, 0 when anonymous.
    pub zone_id: u32,
    pub guild: String,
    pub anon: bool,
}

/// `observe.inventory`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Inventory {
    pub count: usize,
    pub items: Vec<InventoryItem>,
    pub currency: Currency,
    /// False when `currency` may not match the server balance, for example while a purchase is in flight.
    pub coin_verified: bool,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InventoryItem {
    pub slot: i32,
    pub item_id: u32,
    pub name: String,
    pub icon: u32,
    pub charges: i32,
    pub idfile: String,
    pub click_spell_id: u32,
    /// Text file id of a readable book or note, absent for other items.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub filename: String,
    pub item_type: u8,
}

/// `observe.messages`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Messages {
    pub count: usize,
    pub messages: Vec<Message>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub kind: String,
    pub text: String,
    pub keywords: Vec<String>,
    pub item_links: Vec<ItemLink>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemLink {
    pub text: String,
    pub item_id: u32,
    pub is_saylink: bool,
}

/// `observe.dialogue`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dialogue {
    pub count: usize,
    pub choices: Vec<DialogueChoice>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DialogueChoice {
    pub index: usize,
    pub text: String,
}

/// `observe.spells`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Spells {
    pub gems: Vec<Gem>,
    pub snapshot_age_ms: u64,
}

/// One memorized gem. An empty gem has every field but `gem` unset, and a spell that the spell
/// table does not know has only `spell_id` set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gem {
    pub gem: usize,
    pub spell_id: Option<u32>,
    pub name: Option<String>,
    pub mana_cost: Option<i32>,
    pub cast_time_ms: Option<u32>,
    pub recast_time_ms: Option<u32>,
}

/// `observe.skills`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skills {
    pub skills: Vec<Skill>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Skill {
    pub id: usize,
    pub name: Option<String>,
    pub value: u32,
}

/// `observe.doors`. An empty `doors` does not mean the zone has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Doors {
    pub doors: Vec<Door>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Door {
    pub door_id: u8,
    pub name: String,
    pub x: f32,
    pub y: f32,
    pub z: f32,
    pub heading: f32,
    pub opentype: u8,
    pub is_open: bool,
}

/// `observe.zone_entrances`. An empty list does not mean the zone has none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneEntrances {
    pub zone_entrances: Vec<ZoneEntrance>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneEntrance {
    pub iterator: u32,
    pub server_x: f32,
    pub server_y: f32,
    pub server_z: f32,
    pub heading: f32,
    pub zone_id: u16,
}

/// `observe.zone_exits`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneExits {
    pub zone_exits: Vec<ZoneExit>,
    pub snapshot_age_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneExit {
    pub index: i32,
    /// Destination zone, `None` when the exit region is not advertised in the entrance list.
    pub zone_id: Option<u16>,
    /// Point inside the exit region nearest the player.
    pub location: Option<[f32; 3]>,
    /// True when the client refuses to cross this exit.
    pub gated: bool,
}

/// A refusal body that carries more than a message. Every other refusal body is a string.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum Unavailable {
    /// The loaded zone assets cannot describe the zone that the character is in.
    ZoneAssetsNotReady {
        reason: String,
        zone_assets: ZoneAssets,
        message: String,
    },
    /// The zone region map that the answer comes from is not loaded.
    ZoneRegionDataUnavailable {
        reason: String,
        detail: String,
        message: String,
    },
}

/// What the client has loaded for the zone, as `observe.*` refusals report it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneAssets {
    /// One of `idle`, `pending`, `ready`, `failed`, `stale` or `unknown_zone`.
    pub state: String,
    pub reason: Option<String>,
    /// Zone that the loaded or loading assets are for.
    pub zone: Option<String>,
    /// Zone that the client believes the character is in.
    pub player_zone: Option<String>,
    pub status: Option<String>,
    pub terrain_meshes: Option<usize>,
    pub collision_loaded: bool,
    pub detail: String,
}

/// A refusal that names the action it refused.
///
/// `request_id` is the id that the action event feed reports for the same action. It is absent when
/// the call was refused before the host admitted an action.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Refused {
    pub status: RefusedStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<u64>,
    pub reason: String,
}

/// The `status` word of a [`Refused`] body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RefusedStatus {
    Refused,
}

impl Refused {
    /// Constructs a new [`Refused`] without an action id, for `reason`.
    pub fn new(reason: impl Into<String>) -> Self {
        Self { status: RefusedStatus::Refused, request_id: None, reason: reason.into() }
    }
}

/// The body of a refused call.
#[derive(Debug, Clone, PartialEq)]
pub enum ErrorBody {
    Message(String),
    Refused(Refused),
    Unavailable(Unavailable),
}

impl ErrorBody {
    /// Reads `body` as an error body, or returns the reason that it is a string, a [`Refused`] and
    /// an [`Unavailable`] at once.
    pub fn decode(body: Value) -> Result<Self, serde_json::Error> {
        match body {
            Value::String(message) => Ok(Self::Message(message)),
            Value::Object(map) if map.contains_key("status") => {
                serde_json::from_value(Value::Object(map)).map(Self::Refused)
            }
            other => serde_json::from_value(other).map(Self::Unavailable),
        }
    }

    /// The JSON body that goes on the wire.
    pub fn to_body(&self) -> Value {
        match self {
            Self::Message(message) => Value::String(message.clone()),
            Self::Refused(r) => serde_json::to_value(r).expect("an error body always serializes"),
            Self::Unavailable(u) => serde_json::to_value(u).expect("an error body always serializes"),
        }
    }
}

/// The typed body of a successful call.
///
/// A `Reply` serializes as its payload alone. The variant is not on the wire, so a caller recovers
/// it with [`Reply::decode`] and the `Op` that it sent.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Reply {
    Text(String),
    Receipt(Receipt),
    GroupRoster(GroupRoster),
    GuildRoster(GuildRoster),
    MerchantList(MerchantList),
    QuestLog(QuestLog),
    QuestsCompleted(QuestsCompleted),
    QuestOffers(QuestOffers),
    Friends(Friends),
    TrainerList(TrainerList),
    Events(Events),
    ItemText(ItemText),
    Who(Who),
    Inventory(Inventory),
    Messages(Messages),
    Dialogue(Dialogue),
    Spells(Spells),
    Skills(Skills),
    Doors(Doors),
    ZoneEntrances(ZoneEntrances),
    ZoneExits(ZoneExits),
}

macro_rules! reply_from {
    ($($ty:ident),* $(,)?) => {
        $(impl From<$ty> for Reply {
            fn from(value: $ty) -> Self {
                Self::$ty(value)
            }
        })*
    };
}

reply_from!(
    Receipt, GroupRoster, GuildRoster, MerchantList, QuestLog, QuestsCompleted, QuestOffers, Friends,
    TrainerList, Events, ItemText, Who, Inventory, Messages, Dialogue, Spells, Skills, Doors,
    ZoneEntrances, ZoneExits,
);

impl From<String> for Reply {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<&str> for Reply {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<Unavailable> for ErrorBody {
    fn from(value: Unavailable) -> Self {
        Self::Unavailable(value)
    }
}

impl From<Refused> for ErrorBody {
    fn from(value: Refused) -> Self {
        Self::Refused(value)
    }
}

impl From<String> for ErrorBody {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for ErrorBody {
    fn from(message: &str) -> Self {
        Self::Message(message.to_owned())
    }
}

impl Reply {
    /// Reads `body` as the reply of `op`, or returns the reason that it does not fit.
    ///
    /// A tracked op reads as a [`Receipt`], and an op that answers with an acknowledgement reads as
    /// [`Reply::Text`].
    pub fn decode(op: &Op, body: Value) -> Result<Self, serde_json::Error> {
        fn read<T: serde::de::DeserializeOwned>(body: Value, wrap: fn(T) -> Reply) -> Result<Reply, serde_json::Error> {
            serde_json::from_value(body).map(wrap)
        }
        match op {
            Op::GroupRoster => read(body, Reply::GroupRoster),
            Op::GuildRoster => read(body, Reply::GuildRoster),
            Op::MerchantList => read(body, Reply::MerchantList),
            Op::QuestsLog => read(body, Reply::QuestLog),
            Op::QuestsCompleted => read(body, Reply::QuestsCompleted),
            Op::QuestsOffers => read(body, Reply::QuestOffers),
            Op::SocialFriends => read(body, Reply::Friends),
            Op::TrainerList => read(body, Reply::TrainerList),
            Op::EventsRead(_) => read(body, Reply::Events),
            Op::ObserveItemText => read(body, Reply::ItemText),
            Op::ObserveWho => read(body, Reply::Who),
            Op::ObserveInventory => read(body, Reply::Inventory),
            Op::ObserveMessages(_) => read(body, Reply::Messages),
            Op::ObserveDialogue => read(body, Reply::Dialogue),
            Op::ObserveSpells => read(body, Reply::Spells),
            Op::ObserveSkills => read(body, Reply::Skills),
            Op::ObserveDoors => read(body, Reply::Doors),
            Op::ObserveZoneEntrances => read(body, Reply::ZoneEntrances),
            Op::ObserveZoneExits => read(body, Reply::ZoneExits),
            Op::SocialEditFriends(_) => read(body, Reply::Text),
            op if op.is_tracked() => read(body, Reply::Receipt),
            _ => read(body, Reply::Text),
        }
    }

    /// The JSON body that goes on the wire.
    pub fn to_body(&self) -> Value {
        serde_json::to_value(self).expect("a reply always serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn round_trip(op: Op, body: Value) {
        let reply = Reply::decode(&op, body.clone()).expect("body decodes");
        assert_eq!(reply.to_body(), body);
    }

    #[test]
    fn receipts_keep_their_wire_shape() {
        round_trip(Op::GroupAccept, json!({"status":"accepted","request_id":3,"message":"queued"}));
        round_trip(
            Op::MerchantOpen(crate::op::MerchantOpen { merchant: "Vendor".into() }),
            json!({"status":"open","request_id":4,"merchant_id":77}),
        );
        round_trip(
            Op::InteractGive(crate::op::InteractGive { npc: "Guard".into(), from: 23 }),
            json!({"status":"unconfirmed","request_id":5,"message":"no echo","npc":"Guard","from":23}),
        );
    }

    #[test]
    fn receipt_reports_confirmation() {
        let bought = Receipt::Bought {
            request_id: None,
            item: "Torch".into(),
            price: 4,
            coin_after: Currency { platinum: 1, gold: 2, silver: 3, copper: 4 },
        };
        assert!(bought.is_confirmed());
        assert_eq!(bought.reason(), "bought");
        let pending = Receipt::Accepted { request_id: None, message: "queued".into() };
        assert!(!pending.is_confirmed());
        assert_eq!(pending.with_request_id(9), Receipt::Accepted { request_id: Some(9), message: "queued".into() });
    }

    #[test]
    fn reads_keep_their_wire_shape() {
        round_trip(
            Op::GuildRoster,
            json!({"guild":"G","guild_id":4,"guild_rank":1,"pending_invite":null,"members":[
                {"name":"A","rank":1,"level":10,"class":1,"zone_id":0,"online":false,"public_note":""}]}),
        );
        round_trip(
            Op::ObserveSpells,
            json!({"gems":[{"gem":0,"spell_id":null,"name":null,"mana_cost":null,"cast_time_ms":null,"recast_time_ms":null}],
                   "snapshot_age_ms":12}),
        );
        round_trip(
            Op::ObserveZoneExits,
            json!({"zone_exits":[{"index":2,"zone_id":null,"location":[1.0,2.0,3.0],"gated":true}],"snapshot_age_ms":0}),
        );
    }

    #[test]
    fn locked_task_objective_has_no_counts() {
        let task: Task = serde_json::from_value(json!({
            "task_id":1,"title":"t","description":"d","xp_reward":0,"coin_reward":0,"reward_item_text":"",
            "status":"Active","sequence_number":0,"activities":[{"activity_id":2,"state":"locked","optional":false}]
        })).unwrap();
        assert_eq!(task.activities[0].progress, ActivityProgress::Locked { optional: false });
    }

    #[test]
    fn a_body_that_does_not_fit_the_op_is_an_error() {
        assert!(Reply::decode(&Op::GroupRoster, json!("text")).is_err());
    }

    #[test]
    fn refusals_are_strings_or_typed_unavailable() {
        let message = ErrorBody::decode(json!("nope")).unwrap();
        assert_eq!(message, ErrorBody::Message("nope".into()));
        let refused = json!({"status":"refused","request_id":8,"reason":"no"});
        assert_eq!(ErrorBody::decode(refused.clone()).unwrap().to_body(), refused);
        let body = json!({"error":"zone_region_data_unavailable","reason":"r","detail":"d","message":"m"});
        let typed = ErrorBody::decode(body.clone()).unwrap();
        assert!(matches!(typed, ErrorBody::Unavailable(Unavailable::ZoneRegionDataUnavailable { .. })));
        assert_eq!(typed.to_body(), body);
    }
}
