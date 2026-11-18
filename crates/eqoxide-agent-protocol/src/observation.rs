//! Per-tick push from eqoxide to the agent (spec §9). Pushed unconditionally every tick,
//! independent of whether a `Step` arrived (spec §5).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CastingView {
    pub spell_id: u32,
    pub elapsed_ms: u32,
    pub cast_ms: u32,
}

/// Mirrors `eqoxide_core::game_state::BuffSlot` plus its map key (spec §9, #1127). `duration_ticks`
/// stays signed straight through the wire: EQEmu writes `-1000` for a permanent buff, and treating
/// that as unsigned would publish a fabricated-looking ~4.29 billion tick count instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuffView {
    pub slot: u32,
    pub spell_id: u32,
    pub duration_ticks: i32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OwnState {
    pub pos: [f32; 3],
    pub heading: f32,
    pub hp: i32,
    pub hp_max: i32,
    /// Mirrors `GameState::hp_verified()`: false during the estimate-only window before the
    /// server's first self `OP_HPUpdate`, so the agent never mistakes a guess for a confirmed
    /// reading — same "false, not a confident 0/guess" contract as `endurance_confirmed` below.
    pub hp_verified: bool,
    pub mana: i32,
    pub mana_max: i32,
    /// #1127: mirrors `GameState.cur_endurance`/`max_endurance`.
    pub endurance: i32,
    pub endurance_max: i32,
    /// #1127: mirrors `GameState.endurance_confirmed` — false (not a confident 0) until at least
    /// one `OP_EnduranceUpdate` has been seen.
    pub endurance_confirmed: bool,
    pub casting: Option<CastingView>,
    /// #1127: every occupied buff slot, mirroring `GameState.buffs`. Always present (empty when no
    /// buffs are active), sorted by slot (the source `BTreeMap`'s natural iteration order).
    pub buffs: Vec<BuffView>,
    pub zone_name: String,
    /// Mirrors `GameState.target_id`.
    pub target_id: Option<u32>,
    /// Mirrors `GameState.target_name`.
    pub target_name: Option<String>,
    /// Mirrors `GameState.auto_attack`.
    pub auto_attack: bool,
    /// Mirrors `GameState.sitting` — true when the player is sitting.
    pub sitting: bool,
    /// True iff the local controller is frozen (`GameState.player_hold.is_some()`) — e.g. embedded
    /// or underworld with no recovery. Deliberately NOT the raw `ControllerHold`/
    /// `ControllerHoldReason` type: those are internal debugging types with no `Serialize`, not
    /// designed for the wire. `held: true` is the actual fix for the underlying problem: without
    /// it, an agent that sends movement into a frozen controller gets zero signal that its
    /// movement is being silently dropped (the agent API's closest analogue refuses this
    /// pre-emptively via `require_live_session`/`MoveGate`) — this lets the agent see it and
    /// decide whether to keep trying or do something else.
    pub held: bool,
    /// The id of the moving surface in [`Observation::surfaces`] that the character stands on, or
    /// `None` on static ground or in the air. A change of this value between two ticks is the
    /// character boarding or leaving that surface.
    #[serde(default)]
    pub standing_on: Option<u32>,
    /// Mirrors `GameState.player_class`.
    pub player_class: String,
    /// Mirrors `GameState.player_level`.
    pub player_level: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VisibleEntity {
    pub spawn_id: u32,
    pub name: String,
    pub is_npc: bool,
    pub level: u32,
    pub race: String,
    pub pos: [f32; 3],
    pub heading: f32,
    pub hp_pct: f32,
    pub dead: bool,
    /// Whether the entity is a boat, whatever its body type.
    #[serde(default)]
    pub is_boat: bool,
    /// Contains the velocity of the entity in client coordinates as units per second
    /// `[east, north, up]`, measured from its position updates, and reads all zero while the entity
    /// is at rest or has stopped sending updates.
    #[serde(default)]
    pub velocity: [f32; 3],
}

/// Raw SPA effect ids (`SPA_BLANK = 254` slots already filtered out by whoever builds this) and a
/// raw EQ target-type code, rather than a hand-curated enum — matches the precedent set by
/// `eqoxide_core::spells::SpellInfo` itself (which only names `ST_SELF = 6`, no enum).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AbilityFeature {
    pub gem: u8,
    pub spell_id: u32,
    pub target_type: u8,
    pub effects: Vec<i32>,
    /// #1127: mirrors `SpellInfo.mana_cost` — signed; some clicks/procs cost negative mana.
    pub mana_cost: i32,
    /// #1127: mirrors `SpellInfo.cast_time_ms`.
    pub cast_time_ms: u32,
    /// #1127: mirrors `SpellInfo.recast_time_ms`.
    pub recast_time_ms: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LegalActionMask {
    pub gems: [bool; 9],
    pub abilities: Vec<AbilityFeature>,
}

/// What kind of moving surface a [`SurfaceView`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKindView {
    /// A platform that moves only vertically, such as a Kelethin lift.
    Platform,
    /// A vessel that moves horizontally.
    Boat,
}

/// A moving surface of the current zone, such as a lift platform. Its position and velocity let
/// an agent tell a platform that is moving from one at rest and plan around the wait.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceView {
    /// The surface id, which `OwnState::standing_on` refers to. A door surface uses the door id.
    pub id: u32,
    pub kind: SurfaceKindView,
    /// The center of the surface in client coordinates `[east, north, up]`.
    pub pos: [f32; 3],
    /// The largest horizontal distance from `pos` to the edge of the surface.
    pub footprint_radius: f32,
    /// Units per second in client coordinates, all zero while the surface is at rest.
    pub velocity: [f32; 3],
    /// False when a character standing on the surface does not move with it. A rider is not
    /// carried by a surface that reports false, so the agent must not plan to ride it.
    pub carries_riders: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub own: OwnState,
    pub visible: Vec<VisibleEntity>,
    /// Every moving surface of the zone, nearest the character first. Empty when the zone has
    /// none or its collision is not loaded.
    #[serde(default)]
    pub surfaces: Vec<SurfaceView>,
    pub legal_actions: LegalActionMask,
    /// Character death as ongoing STATE, not a terminal boundary (spec §9) — routine in combat
    /// training; the agent keeps receiving observations while dead and must issue
    /// `Lifecycle::Respawn` to continue.
    pub dead: bool,
    /// True session end — eqoxide can only ever honestly assert this when the net thread has died
    /// for good (see `eqoxide-agent-plugin-host`'s `observation_builder`, Task 13).
    pub terminated: bool,
    /// An artificial, harness-imposed rollout cutoff. eqoxide never imposes one itself in v1 — this
    /// is always `false` from eqoxide's side; an external driver may still truncate its own
    /// rollout without needing this field to say so.
    pub truncated: bool,
    /// False while no zone collision geometry is loaded (startup, zone transitions, or
    /// `--testzone`'s synthetic debug zone) — in that window `visible` is always `[]` regardless of
    /// what's actually nearby, and that's NOT the same claim as "nothing is visible." True once
    /// real geometry backs the visibility filter's line-of-sight checks.
    pub visibility_available: bool,
    /// Monotonically increasing per-connection tick counter, starting at 0 on the first
    /// Observation after a successful handshake. Lets the agent detect dropped/bursted ticks and
    /// reconstruct elapsed time, since there is no per-Step acknowledgment (Observation is the
    /// only outbound message shape).
    pub tick: u64,
    /// The answers decided since the previous `Observation` to verbs whose `Step` carried a `seq`:
    /// the verb dispatched this tick, plus any refused at intake (queue full, a `seq` with no
    /// verb). Each verb is answered exactly once. Empty, and absent from the wire, on every other
    /// `Observation`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verb_results: Vec<crate::verb_result::VerbResult>,
    /// The replies to `Call`s that finished since the previous `Observation`, in the order they
    /// finished. Empty, and absent from the wire, on every other `Observation`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub call_replies: Vec<crate::call::CallReply>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framing::{decode_line, encode_line};

    fn round_trips(o: &Observation) {
        let line = encode_line(o).unwrap();
        let back: Observation = decode_line(&line).unwrap();
        assert_eq!(&back, o);
    }

    #[test]
    fn observation_with_casting_and_visible_entities_round_trips() {
        let o = Observation {
            own: OwnState {
                pos: [1.0, 2.0, 3.0],
                heading: 45.0,
                hp: 100,
                hp_max: 100,
                hp_verified: true,
                mana: 50,
                mana_max: 100,
                endurance: 80,
                endurance_max: 100,
                endurance_confirmed: true,
                casting: Some(CastingView {
                    spell_id: 12,
                    elapsed_ms: 500,
                    cast_ms: 3000,
                }),
                buffs: vec![
                    BuffView {
                        slot: 3,
                        spell_id: 90,
                        duration_ticks: -1000,
                    },
                    BuffView {
                        slot: 5,
                        spell_id: 12,
                        duration_ticks: 42,
                    },
                ],
                zone_name: "qeynos".into(),
                target_id: Some(7),
                target_name: Some("a_rat00".into()),
                auto_attack: true,
                sitting: false,
                held: false,
                standing_on: Some(3),
                player_class: "Warrior".into(),
                player_level: 10,
            },
            surfaces: vec![SurfaceView {
                id: 3,
                kind: SurfaceKindView::Platform,
                pos: [10.0, 20.0, 30.0],
                footprint_radius: 25.0,
                velocity: [0.0, 0.0, 9.0],
                carries_riders: true,
            }],
            visible: vec![VisibleEntity {
                spawn_id: 7,
                name: "a_rat00".into(),
                is_npc: true,
                level: 3,
                race: "Rat".into(),
                pos: [4.0, 5.0, 6.0],
                heading: 0.0,
                hp_pct: 75.0,
                dead: false,
                is_boat: true,
                velocity: [1.25, 0.0, 0.0],
            }],
            legal_actions: LegalActionMask {
                gems: [true, false, false, false, false, false, false, false, false],
                abilities: vec![AbilityFeature {
                    gem: 0,
                    spell_id: 12,
                    target_type: 5,
                    effects: vec![79],
                    mana_cost: 25,
                    cast_time_ms: 3000,
                    recast_time_ms: 0,
                }],
            },
            dead: false,
            terminated: false,
            truncated: false,
            visibility_available: true,
            tick: 0,
            verb_results: vec![],
            call_replies: vec![],
        };
        round_trips(&o);
    }

    #[test]
    fn an_observation_without_surface_fields_still_decodes() {
        let o = Observation {
            own: OwnState {
                pos: [0.0; 3], heading: 0.0, hp: 1, hp_max: 1, hp_verified: false, mana: 0, mana_max: 0,
                endurance: 0, endurance_max: 0, endurance_confirmed: false, casting: None, buffs: vec![],
                zone_name: "gfaydark".into(), target_id: None, target_name: None, auto_attack: false,
                sitting: false, held: false, standing_on: Some(3), player_class: "Warrior".into(),
                player_level: 1,
            },
            surfaces: vec![], visible: vec![],
            legal_actions: LegalActionMask { gems: [false; 9], abilities: vec![] },
            dead: false, terminated: false, truncated: false, visibility_available: true, tick: 0,
            verb_results: vec![], call_replies: vec![],
        };
        let mut json: serde_json::Value = serde_json::to_value(&o).unwrap();
        json.as_object_mut().unwrap().remove("surfaces");
        json["own"].as_object_mut().unwrap().remove("standing_on");
        let back: Observation = serde_json::from_value(json).unwrap();
        assert!(back.surfaces.is_empty());
        assert_eq!(back.own.standing_on, None);
    }

    #[test]
    fn visible_entity_without_boat_fields_decodes_with_defaults() {
        let o = Observation {
            own: OwnState {
                pos: [0.0; 3], heading: 0.0, hp: 1, hp_max: 1, hp_verified: false, mana: 0, mana_max: 0,
                endurance: 0, endurance_max: 0, endurance_confirmed: false, casting: None, buffs: vec![],
                zone_name: "freporte".into(), target_id: None, target_name: None, auto_attack: false,
                sitting: false, held: false, standing_on: None, player_class: "Warrior".into(),
                player_level: 1,
            },
            surfaces: vec![],
            visible: vec![VisibleEntity {
                spawn_id: 9,
                name: "a_ferry".into(),
                is_npc: true,
                level: 1,
                race: "Boat".into(),
                pos: [1.0, 2.0, 3.0],
                heading: 0.0,
                hp_pct: 100.0,
                dead: false,
                is_boat: true,
                velocity: [4.0, 0.0, 0.0],
            }],
            legal_actions: LegalActionMask { gems: [false; 9], abilities: vec![] },
            dead: false, terminated: false, truncated: false, visibility_available: true, tick: 0,
            verb_results: vec![], call_replies: vec![],
        };
        let mut json: serde_json::Value = serde_json::to_value(&o).unwrap();
        let entity = json["visible"][0].as_object_mut().unwrap();
        entity.remove("is_boat");
        entity.remove("velocity");
        let back: Observation = serde_json::from_value(json).unwrap();
        assert!(!back.visible[0].is_boat);
        assert_eq!(back.visible[0].velocity, [0.0; 3]);
    }

    #[test]
    fn observation_with_no_casting_and_no_visible_entities_round_trips() {
        let o = Observation {
            own: OwnState {
                pos: [0.0, 0.0, 0.0],
                heading: 0.0,
                hp: 0,
                hp_max: 100,
                hp_verified: false,
                mana: 0,
                mana_max: 0,
                endurance: 0,
                endurance_max: 0,
                endurance_confirmed: false,
                casting: None,
                buffs: vec![],
                zone_name: "qeynos".into(),
                target_id: None,
                target_name: None,
                auto_attack: false,
                sitting: false,
                held: false,
                standing_on: None,
                player_class: "Warrior".into(),
                player_level: 1,
            },
            surfaces: vec![],
            visible: vec![],
            legal_actions: LegalActionMask {
                gems: [false; 9],
                abilities: vec![],
            },
            dead: true,
            terminated: false,
            truncated: false,
            visibility_available: false,
            tick: 0,
            verb_results: vec![],
            call_replies: vec![],
        };
        round_trips(&o);
    }
}
