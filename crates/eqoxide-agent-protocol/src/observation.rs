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
    /// movement is being silently dropped (the HTTP API's closest analogue refuses this
    /// pre-emptively via `require_live_session`/`MoveGate`) — this lets the agent see it and
    /// decide whether to keep trying or do something else.
    pub held: bool,
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub own: OwnState,
    pub visible: Vec<VisibleEntity>,
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
                casting: Some(CastingView { spell_id: 12, elapsed_ms: 500, cast_ms: 3000 }),
                buffs: vec![
                    BuffView { slot: 3, spell_id: 90, duration_ticks: -1000 },
                    BuffView { slot: 5, spell_id: 12, duration_ticks: 42 },
                ],
                zone_name: "qeynos".into(),
                target_id: Some(7),
                target_name: Some("a_rat00".into()),
                auto_attack: true,
                sitting: false,
                held: false,
                player_class: "Warrior".into(),
                player_level: 10,
            },
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
        };
        round_trips(&o);
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
                player_class: "Warrior".into(),
                player_level: 1,
            },
            visible: vec![],
            legal_actions: LegalActionMask { gems: [false; 9], abilities: vec![] },
            dead: true,
            terminated: false,
            truncated: false,
            visibility_available: false,
            tick: 0,
        };
        round_trips(&o);
    }
}
