//! Every operation an agent can ask eqoxide for through a [`Call`](crate::call::Call), with the
//! arguments each one takes.
//!
//! An op is named `"<group>.<action>"` on the wire and carries its arguments as one object in
//! `args`. An op that takes no arguments omits `args`. A name that is not in [`Op`] or an argument
//! that does not fit its struct is answered `Invalid` with the path to the failing field.
//!
//! [`Op`] is externally tagged for serde. [`Call`](crate::call::Call) writes the tag as `op` and
//! the content as `args`.

use serde::{Deserialize, Serialize};

/// One operation and its arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Op {
    #[serde(rename = "group.roster")]
    GroupRoster,
    #[serde(rename = "group.invite")]
    GroupInvite(Named),
    #[serde(rename = "group.accept")]
    GroupAccept,
    #[serde(rename = "group.decline")]
    GroupDecline,
    #[serde(rename = "group.leave")]
    GroupLeave,
    #[serde(rename = "group.kick")]
    GroupKick(Named),
    #[serde(rename = "group.makeleader")]
    GroupMakeLeader(Named),

    #[serde(rename = "guild.roster")]
    GuildRoster,
    #[serde(rename = "guild.invite")]
    GuildInvite(Named),
    #[serde(rename = "guild.accept")]
    GuildAccept,
    #[serde(rename = "guild.leave")]
    GuildLeave,
    #[serde(rename = "guild.remove")]
    GuildRemove(Named),

    #[serde(rename = "merchant.open")]
    MerchantOpen(MerchantOpen),
    #[serde(rename = "merchant.close")]
    MerchantClose,
    #[serde(rename = "merchant.list")]
    MerchantList,
    #[serde(rename = "merchant.buy")]
    MerchantBuy(MerchantBuy),
    #[serde(rename = "merchant.sell")]
    MerchantSell(MerchantSell),

    #[serde(rename = "quests.log")]
    QuestsLog,
    #[serde(rename = "quests.completed")]
    QuestsCompleted,
    #[serde(rename = "quests.offers")]
    QuestsOffers,
    #[serde(rename = "quests.accept")]
    QuestsAccept(TaskRef),
    #[serde(rename = "quests.decline")]
    QuestsDecline,
    #[serde(rename = "quests.cancel")]
    QuestsCancel(TaskRef),

    #[serde(rename = "social.friends")]
    SocialFriends,
    #[serde(rename = "social.edit_friends")]
    SocialEditFriends(FriendsEdit),

    #[serde(rename = "trainer.open")]
    TrainerOpen(TrainerOpen),
    #[serde(rename = "trainer.list")]
    TrainerList,
    #[serde(rename = "trainer.train")]
    TrainerTrain(TrainerTrain),
    #[serde(rename = "trainer.close")]
    TrainerClose,

    #[serde(rename = "combat.memorize")]
    CombatMemorize(CombatMemorize),
    #[serde(rename = "combat.scribe")]
    CombatScribe(CombatScribe),

    #[serde(rename = "interact.hail")]
    InteractHail(InteractHail),
    #[serde(rename = "interact.say")]
    InteractSay(InteractSay),
    #[serde(rename = "interact.loot")]
    InteractLoot(InteractLoot),
    #[serde(rename = "interact.give")]
    InteractGive(InteractGive),
    #[serde(rename = "interact.click_door")]
    InteractClickDoor(InteractClickDoor),
    #[serde(rename = "interact.run")]
    InteractRun,
    #[serde(rename = "interact.walk")]
    InteractWalk,
    #[serde(rename = "interact.dialogue")]
    InteractDialogue(InteractDialogue),
    #[serde(rename = "interact.read")]
    InteractRead(InteractRead),

    #[serde(rename = "observe.inventory")]
    ObserveInventory,
    #[serde(rename = "observe.messages")]
    ObserveMessages(ObserveMessages),
    #[serde(rename = "observe.dialogue")]
    ObserveDialogue,
    #[serde(rename = "observe.spells")]
    ObserveSpells,
    #[serde(rename = "observe.skills")]
    ObserveSkills,
    #[serde(rename = "observe.doors")]
    ObserveDoors,
    #[serde(rename = "observe.zone_entrances")]
    ObserveZoneEntrances,
    #[serde(rename = "observe.zone_exits")]
    ObserveZoneExits,
    #[serde(rename = "observe.item_text")]
    ObserveItemText,
    #[serde(rename = "observe.who")]
    ObserveWho,

    #[serde(rename = "events.read")]
    EventsRead(EventsRead),

    /// A call whose `op` or `args` did not parse, with the reason. The host answers `Invalid`. A
    /// caller never builds this variant and it is never written to the wire.
    #[serde(skip)]
    Unreadable(String),
}

/// A player to act on, by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Named {
    pub name: String,
}

/// `merchant.open`: open the window of a nearby merchant.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchantOpen {
    /// Merchant NPC name, fuzzy-matched.
    pub merchant: String,
}

/// `merchant.buy`: buy the item in one merchant slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchantBuy {
    /// Merchant NPC name, fuzzy-matched.
    pub merchant: String,
    /// Merchant inventory slot of the item, from `merchant.list`.
    pub slot: u32,
}

/// `merchant.sell`: sell the item in one inventory slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MerchantSell {
    /// Merchant NPC name, fuzzy-matched.
    pub merchant: String,
    /// Player inventory wire slot of the item, as `observe.inventory` reports it.
    pub slot: u32,
    /// Number to sell from the slot. Defaults to 1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantity: Option<u32>,
}

/// A task, by id.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRef {
    pub task_id: u32,
}

/// `social.edit_friends`: add or remove one name. Exactly one of the two is set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FriendsEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub add: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remove: Option<String>,
}

/// `trainer.open`: open the skill window of a nearby guildmaster. Exactly one of the two names is
/// set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainerOpen {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trainer: Option<String>,
}

/// `trainer.train`: train one point of a skill at the open trainer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainerTrain {
    pub skill_id: u32,
}

/// `combat.memorize`: memorize a scribed spell into a gem.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombatMemorize {
    pub spell_id: u32,
    /// Gem index, 0 to 8.
    pub gem: u32,
}

/// `combat.scribe`: scribe a spell scroll into the spellbook.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CombatScribe {
    pub spell_id: u32,
    /// Spellbook slot. Defaults to 0.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u32>,
    /// Inventory wire slot of the scroll. Omit only when the scroll is already on the cursor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<u32>,
}

/// `interact.hail`: hail an NPC.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractHail {
    /// NPC name, fuzzy-matched. Omit to hail the nearest NPC.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `interact.say`: say text on the Say channel.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractSay {
    pub text: String,
}

/// `interact.loot`: loot a corpse. At most one of the two is set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractLoot {
    /// Corpse spawn id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<u32>,
    /// Corpse name, fuzzy-matched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `interact.give`: give the item in one inventory slot to an NPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractGive {
    /// NPC name, fuzzy-matched.
    pub npc: String,
    /// Inventory wire slot of the item.
    pub from: u32,
}

/// `interact.click_door`: click a door. Exactly one of the two is set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractClickDoor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub door_id: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// `interact.dialogue`: answer an open dialogue. Exactly one of the two is set.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractDialogue {
    /// Index into `observe.dialogue`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// `interact.read`: read the book or note in one inventory slot.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InteractRead {
    pub slot: i32,
}

/// `observe.messages`: read the message log.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObserveMessages {
    /// Only lines of this kind, such as `npc`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

/// A category of the event feed, the bucket an event is filed under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventCategory {
    Action,
    Chat,
    Combat,
    Group,
    Loot,
    Merchant,
    Navigate,
}

impl EventCategory {
    /// The name an event carries in its `category` field.
    pub fn as_str(self) -> &'static str {
        match self {
            EventCategory::Action => "action",
            EventCategory::Chat => "chat",
            EventCategory::Combat => "combat",
            EventCategory::Group => "group",
            EventCategory::Loot => "loot",
            EventCategory::Merchant => "merchant",
            EventCategory::Navigate => "navigate",
        }
    }
}

/// `events.read`: read the async event feed.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventsRead {
    /// Only events in this category.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<EventCategory>,
    /// Only events with an id greater than this cursor. Ids are 1-based.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
    /// Block up to this many seconds, capped at 30, for a matching event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait: Option<u64>,
    /// 1 for only events addressed to the player.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub directed: Option<u8>,
}

/// Why [`Op::from_wire`] could not read an op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadError {
    /// The name is not an op.
    UnknownOp(String),
    /// The name is an op and its arguments do not fit, with the path to the failing field.
    Arguments(String),
}

impl std::fmt::Display for ReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadError::UnknownOp(name) => write!(f, "unknown op `{name}`"),
            ReadError::Arguments(why) => f.write_str(why),
        }
    }
}

impl std::error::Error for ReadError {}

impl Op {
    /// Whether the op changes the session, so that its outcome is tracked. Reads and the
    /// client-local friends list are not.
    pub fn is_tracked(&self) -> bool {
        matches!(
            self,
            Op::GroupInvite(_) | Op::GroupAccept | Op::GroupDecline | Op::GroupLeave | Op::GroupKick(_)
                | Op::GroupMakeLeader(_) | Op::GuildInvite(_) | Op::GuildAccept | Op::GuildLeave
                | Op::GuildRemove(_) | Op::MerchantOpen(_) | Op::MerchantClose | Op::MerchantBuy(_)
                | Op::MerchantSell(_) | Op::QuestsAccept(_) | Op::QuestsDecline | Op::QuestsCancel(_)
                | Op::TrainerOpen(_) | Op::TrainerTrain(_) | Op::TrainerClose | Op::CombatMemorize(_)
                | Op::CombatScribe(_) | Op::InteractHail(_) | Op::InteractSay(_) | Op::InteractLoot(_)
                | Op::InteractGive(_) | Op::InteractClickDoor(_) | Op::InteractRun | Op::InteractWalk
                | Op::InteractDialogue(_) | Op::InteractRead(_)
        )
    }

    /// Whether a later game event, rather than admission, confirms the outcome of the op.
    pub fn is_awaited(&self) -> bool {
        matches!(self, Op::MerchantOpen(_) | Op::MerchantBuy(_) | Op::InteractGive(_))
    }

    /// Reads the op named `name` with its `args`, or the [`ReadError`] that says why it does not
    /// parse. An argument error names the failing field. An op whose arguments are all optional
    /// reads without `args`, as if `args` were an empty object.
    pub fn from_wire(name: &str, args: Option<serde_json::Value>) -> Result<Op, ReadError> {
        let read = |tagged: serde_json::Value| -> Result<Op, ReadError> {
            serde_path_to_error::deserialize(tagged).map_err(|e| {
                // an error at the root of the tagged value is the tag itself failing to name an op
                if e.path().iter().next().is_none() && e.to_string().contains("unknown variant") {
                    ReadError::UnknownOp(name.to_owned())
                } else {
                    ReadError::Arguments(e.to_string())
                }
            })
        };
        let with_args = |args: serde_json::Value| {
            let mut map = serde_json::Map::new();
            map.insert(name.to_owned(), args);
            serde_json::Value::Object(map)
        };
        match args {
            Some(args) => read(with_args(args)),
            None => read(serde_json::Value::String(name.to_owned()))
                .or_else(|first| read(with_args(serde_json::json!({}))).map_err(|_| first)),
        }
    }

    /// The wire name of this op and its arguments, or `None` for an [`Op::Unreadable`].
    pub fn to_wire(&self) -> Option<(String, Option<serde_json::Value>)> {
        match serde_json::to_value(self).ok()? {
            serde_json::Value::String(name) => Some((name, None)),
            serde_json::Value::Object(map) if map.len() == 1 => {
                let (name, args) = map.into_iter().next()?;
                Some((name, Some(args)))
            }
            _ => None,
        }
    }

    /// The wire name of this op, such as `merchant.buy`. An [`Op::Unreadable`] is named
    /// `unreadable`.
    pub fn name(&self) -> String {
        self.to_wire().map_or_else(|| "unreadable".to_owned(), |(name, _)| name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_op_with_arguments_reads_and_writes_one_object() {
        let op = Op::MerchantBuy(MerchantBuy { merchant: "Vendor".into(), slot: 3 });
        let args = json!({"merchant": "Vendor", "slot": 3});
        assert_eq!(op.to_wire(), Some(("merchant.buy".to_owned(), Some(args.clone()))));
        assert_eq!(Op::from_wire("merchant.buy", Some(args)).unwrap(), op);
    }

    #[test]
    fn an_op_without_arguments_has_no_args() {
        assert_eq!(Op::MerchantList.to_wire(), Some(("merchant.list".to_owned(), None)));
        assert_eq!(Op::from_wire("merchant.list", None).unwrap(), Op::MerchantList);
    }

    #[test]
    fn a_malformed_argument_names_its_field() {
        let err = Op::from_wire("merchant.buy", Some(json!({"merchant": "V", "slot": "x"}))).unwrap_err();
        assert!(err.to_string().contains("slot"), "{err}");
    }

    #[test]
    fn a_misspelled_field_is_refused() {
        assert!(Op::from_wire("merchant.buy", Some(json!({"merchant": "V", "slot": 1, "slott": 2}))).is_err());
    }

    #[test]
    fn an_op_with_only_optional_arguments_reads_without_args() {
        assert_eq!(
            Op::from_wire("interact.hail", None).unwrap(),
            Op::InteractHail(InteractHail::default())
        );
        assert!(Op::from_wire("merchant.buy", None).is_err());
    }

    #[test]
    fn an_unknown_op_is_refused() {
        assert_eq!(Op::from_wire("no.such", None), Err(ReadError::UnknownOp("no.such".into())));
        assert_eq!(
            Op::from_wire("no.such", Some(json!({"a": 1}))),
            Err(ReadError::UnknownOp("no.such".into()))
        );
    }

    #[test]
    fn an_unknown_category_is_an_argument_error_not_an_unknown_op() {
        let err = Op::from_wire("events.read", Some(json!({"category": "bogus"}))).unwrap_err();
        assert!(matches!(&err, ReadError::Arguments(why) if why.contains("category")), "{err}");
    }

    #[test]
    fn every_category_reads_and_names_itself() {
        for name in ["action", "chat", "combat", "group", "loot", "merchant", "navigate"] {
            let op = Op::from_wire("events.read", Some(json!({"category": name}))).unwrap();
            let Op::EventsRead(read) = op else { panic!("not events.read") };
            assert_eq!(read.category.unwrap().as_str(), name);
        }
    }

    #[test]
    fn every_wire_name_round_trips() {
        let names = [
            "group.roster", "guild.roster", "merchant.list", "quests.log", "observe.inventory",
            "observe.who", "interact.run", "trainer.close",
        ];
        for name in names {
            assert_eq!(Op::from_wire(name, None).unwrap().name(), name);
        }
    }

    #[test]
    fn an_unreadable_op_has_no_wire_form() {
        assert_eq!(Op::Unreadable("x".into()).to_wire(), None);
    }
}
