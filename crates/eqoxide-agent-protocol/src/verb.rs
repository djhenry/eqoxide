//! `AgentVerb` — the discrete action layered on top of the continuous movement payload every `Step`
//! carries (spec §7, §8). Mirrors the HTTP API's `/v1/<group>/<action>` taxonomy so wiring a new
//! verb later is additive.

use serde::{Deserialize, Serialize};

/// Mirrors `eqoxide_ipc::CastRequest` (`crates/eqoxide-ipc/src/lib.rs:2514`) field-for-field. A
/// separate type, not a re-export — this crate has zero dependency on eqoxide-ipc (spec §4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CastRequest {
    pub gem: u8,
    pub target_id: Option<u32>,
    pub item_slot: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum CombatVerb {
    Target { spawn_id: u32 },
    Attack { on: bool },
    Consider { spawn_id: u32 },
    Cast(CastRequest),
}

/// Both variants translate to the single real `CommandState::request_sit(bool)` — `Sit` → `true`,
/// `Stand` → `false` (spec §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum InteractVerb {
    Sit,
    Stand,
}

/// `Respawn` dispatches to `CommandState::request_respawn()`, which returns `()` — unlike the other
/// `request_*` methods this dispatcher calls, there is no accepted/refused signal to report back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum LifecycleVerb {
    Respawn,
}

/// Reserved for discrete travel actions like zone crossing — not part of the initial combat scope
/// (spec §8, §12). Typed and constructible now; the plugin host accepts it and does nothing (no
/// handler wired yet) rather than rejecting it as malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action")]
pub enum MoveVerb {
    ZoneCross,
}

/// Reserved, typed, not wired (spec §8, §12). An uninhabited enum — no variant exists to
/// construct — which is a more honest "nothing to see here" than a stub with unread fields: a
/// value of this type can never exist, so there is nothing for a future task to forget to wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MerchantVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InventoryVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QuestsVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatVerb {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Move(MoveVerb),
    Merchant(MerchantVerb),
    Inventory(InventoryVerb),
    Quests(QuestsVerb),
    Chat(ChatVerb),
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
    fn inventory_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Inventory","data":{"action":"Move"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn quests_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Quests","data":{"action":"Accept"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }

    #[test]
    fn chat_verb_cannot_be_deserialized_from_any_action_tag() {
        let result: Result<AgentVerb, _> =
            crate::framing::decode_line(r#"{"verb":"Chat","data":{"action":"Say"}}"#);
        assert!(result.is_err(), "an uninhabited enum must reject every input: {result:?}");
    }
}
