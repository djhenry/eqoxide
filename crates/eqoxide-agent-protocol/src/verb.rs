//! `AgentVerb` — the discrete action layered on top of the continuous movement payload every `Step`
//! carries (spec §7, §8). Mirrors the agent API's `/v1/<group>/<action>` taxonomy so wiring a new
//! verb later is additive.

use serde::{Deserialize, Serialize};

/// Mirrors `eqoxide_ipc::CastRequest` (`crates/eqoxide-ipc/src/lib.rs:2514`) field-for-field. A
/// separate type, not a re-export — this crate has zero dependency on eqoxide-ipc (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CastRequest {
    pub gem: u8,
    pub target_id: Option<u32>,
    pub item_slot: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum CombatVerb {
    Target { spawn_id: u32 },
    Attack { on: bool },
    Consider { spawn_id: u32 },
    Cast(CastRequest),
}

/// Both variants translate to the single real `CommandState::request_sit(bool)` — `Sit` → `true`,
/// `Stand` → `false` (spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum InteractVerb {
    Sit,
    Stand,
}

/// `Respawn` dispatches to `CommandState::request_respawn()`, which returns `()` — unlike the other
/// `request_*` methods this dispatcher calls, there is no accepted/refused signal to report back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum LifecycleVerb {
    Respawn,
    /// Toggle a camp: start one if none is running, cancel the one in progress.
    Camp,
    /// Camp out, then shut the client down cleanly. Honoured even when the network thread is gone
    /// (tearing a dead session down is its job); a watchdog force-exits the process if the camp is
    /// never drained.
    Exit,
}

/// Reserved for discrete travel actions like zone crossing — not part of the initial combat scope
/// (spec §8, §12). Typed and constructible now; the plugin host accepts it and does nothing (no
/// handler wired yet) rather than rejecting it as malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum MoveVerb {
    ZoneCross,
}

/// Reserved, typed, not wired (spec §8, §12). An uninhabited enum — no variant exists to
/// construct — which is a more honest "nothing to see here" than a stub with unread fields: a
/// value of this type can never exist, so there is nothing for a future task to forget to wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MerchantVerb {}

/// Move, equip or unequip one item between wire slots (0-21 worn, 22-29 general, 30 cursor, 251+
/// bag contents). Refused when `from` holds no item in the last published inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum InventoryVerb {
    Move { from: u32, to: u32 },
}

/// One OP_PetCommands byte (the `PET_*` constants in `eqoxide_core::pet`: 2 attack, 4 follow,
/// 5 guard, 6 sit toggle, 28 back off). Name-to-byte mapping is the caller's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum PetVerb {
    Command { command: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestsVerb {}

/// Send one message on an inter-agent channel. `Tell` is a directed whisper (EQ chan 7); the rest
/// are broadcast on their channel (`Ooc` 5, `Shout` 3, `Group` 2, `Guild` 0). Every `text`, and
/// `Tell`'s `to`, must be non-blank or the verb is refused.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
pub enum ChatVerb {
    Tell { to: String, text: String },
    Ooc { text: String },
    Shout { text: String },
    Group { text: String },
    Guild { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "data", deny_unknown_fields)]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Move(MoveVerb),
    Merchant(MerchantVerb),
    Inventory(InventoryVerb),
    Quests(QuestsVerb),
    Chat(ChatVerb),
    Pet(PetVerb),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    fn round_trips(v: AgentVerb) {
        let line = encode_line(&v).unwrap();
        let back: AgentVerb = decode_line(&line).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn combat_target_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 }));
    }

    #[test]
    fn combat_attack_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Attack { on: true }));
    }

    #[test]
    fn combat_consider_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Consider { spawn_id: 7 }));
    }

    #[test]
    fn combat_cast_round_trips() {
        round_trips(AgentVerb::Combat(CombatVerb::Cast(CastRequest {
            gem: 0,
            target_id: Some(42),
            item_slot: None,
        })));
    }

    #[test]
    fn interact_sit_round_trips() {
        round_trips(AgentVerb::Interact(InteractVerb::Sit));
    }

    #[test]
    fn interact_stand_round_trips() {
        round_trips(AgentVerb::Interact(InteractVerb::Stand));
    }

    #[test]
    fn lifecycle_respawn_round_trips() {
        round_trips(AgentVerb::Lifecycle(LifecycleVerb::Respawn));
    }

    #[test]
    fn move_zone_cross_round_trips() {
        round_trips(AgentVerb::Move(MoveVerb::ZoneCross));
    }

    // Golden wire-shape tests: round-trip tests are symmetric by construction and cannot catch a
    // serde `tag`/`content` mismatch between the documented shape and the real one (this is
    // exactly how `CombatVerb` briefly shipped as `#[serde(tag = "action", content = "data")]`,
    // silently double-nesting every combat action's payload). These assert the literal JSON string
    // `docs/agent-api.md` documents for each `AgentVerb` variant.

    #[test]
    fn combat_target_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Combat(CombatVerb::Target { spawn_id: 42 })).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Combat","data":{"action":"Target","spawn_id":42}}"#);
    }

    #[test]
    fn combat_attack_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Combat(CombatVerb::Attack { on: true })).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Combat","data":{"action":"Attack","on":true}}"#);
    }

    #[test]
    fn combat_consider_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Combat(CombatVerb::Consider { spawn_id: 7 })).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Combat","data":{"action":"Consider","spawn_id":7}}"#);
    }

    #[test]
    fn combat_cast_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Combat(CombatVerb::Cast(CastRequest {
            gem: 0,
            target_id: Some(42),
            item_slot: None,
        })))
        .unwrap();
        assert_eq!(
            line.trim_end(),
            r#"{"verb":"Combat","data":{"action":"Cast","gem":0,"target_id":42,"item_slot":null}}"#
        );
    }

    #[test]
    fn interact_sit_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Interact(InteractVerb::Sit)).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Interact","data":{"action":"Sit"}}"#);
    }

    #[test]
    fn lifecycle_respawn_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Lifecycle(LifecycleVerb::Respawn)).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Lifecycle","data":{"action":"Respawn"}}"#);
    }

    #[test]
    fn move_zone_cross_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Move(MoveVerb::ZoneCross)).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Move","data":{"action":"ZoneCross"}}"#);
    }

    #[test]
    fn merchant_verb_cannot_be_deserialized_from_any_action_tag() {
        // No `MerchantVerb` value can ever exist to serialize, so this asserts the negative: no
        // JSON shape deserializes into one, because the enum has no variants to match against.
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Merchant","data":{"action":"Buy"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn lifecycle_camp_and_exit_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Lifecycle(LifecycleVerb::Camp)).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Lifecycle","data":{"action":"Camp"}}"#);
        let line = encode_line(&AgentVerb::Lifecycle(LifecycleVerb::Exit)).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Lifecycle","data":{"action":"Exit"}}"#);
    }

    #[test]
    fn inventory_move_wire_shape_matches_docs() {
        let v = AgentVerb::Inventory(InventoryVerb::Move { from: 23, to: 30 });
        let line = encode_line(&v).unwrap();
        assert_eq!(
            line.trim_end(),
            r#"{"verb":"Inventory","data":{"action":"Move","from":23,"to":30}}"#
        );
        round_trips(v);
    }

    #[test]
    fn pet_command_wire_shape_matches_docs() {
        let v = AgentVerb::Pet(PetVerb::Command { command: 2 });
        let line = encode_line(&v).unwrap();
        assert_eq!(line.trim_end(), r#"{"verb":"Pet","data":{"action":"Command","command":2}}"#);
        round_trips(v);
    }

    #[test]
    fn quests_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Quests","data":{"action":"Accept"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn chat_tell_wire_shape_matches_docs() {
        let line = encode_line(&AgentVerb::Chat(ChatVerb::Tell {
            to: "Alpha".into(),
            text: "hi".into(),
        }))
        .unwrap();
        assert_eq!(
            line.trim_end(),
            r#"{"verb":"Chat","data":{"action":"Tell","to":"Alpha","text":"hi"}}"#
        );
        round_trips(AgentVerb::Chat(ChatVerb::Tell { to: "Alpha".into(), text: "hi".into() }));
    }

    #[test]
    fn chat_channel_verbs_wire_shape_matches_docs() {
        for (v, name) in [
            (ChatVerb::Ooc { text: "x".into() }, "Ooc"),
            (ChatVerb::Shout { text: "x".into() }, "Shout"),
            (ChatVerb::Group { text: "x".into() }, "Group"),
            (ChatVerb::Guild { text: "x".into() }, "Guild"),
        ] {
            let line = encode_line(&AgentVerb::Chat(v.clone())).unwrap();
            assert_eq!(
                line.trim_end(),
                format!(r#"{{"verb":"Chat","data":{{"action":"{name}","text":"x"}}}}"#)
            );
            round_trips(AgentVerb::Chat(v));
        }
    }
}
