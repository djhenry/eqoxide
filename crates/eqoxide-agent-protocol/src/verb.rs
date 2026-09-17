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
#[serde(tag = "action", content = "data")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Move(MoveVerb),
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
}
