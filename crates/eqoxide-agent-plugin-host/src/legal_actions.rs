//! Builds the wire `LegalActionMask` from the character's memorized spells (spec §9). Real data:
//! `GameState.mem_spells` (9 gem slots) plus `SpellDb`, including the per-spell mana cost/cast
//! time/recast delay landed by GitHub issue #1127 (`SpellInfo.mana_cost`/`cast_time_ms`/`recast_time_ms`).

use eqoxide_agent_protocol::observation::{AbilityFeature, LegalActionMask};
use eqoxide_core::game_state::gem_is_empty;
use eqoxide_core::spells::{SpellDb, SPA_BLANK};

pub fn build_legal_actions(mem_spells: &[u32; 9], spell_db: &SpellDb) -> LegalActionMask {
    let mut gems = [false; 9];
    let mut abilities = Vec::new();
    for (i, &spell_id) in mem_spells.iter().enumerate() {
        if gem_is_empty(spell_id) {
            continue;
        }
        gems[i] = true;
        if let Some(info) = spell_db.get(spell_id) {
            let effects: Vec<i32> = info.effects.iter().copied().filter(|&e| e != SPA_BLANK).collect();
            abilities.push(AbilityFeature {
                gem: i as u8,
                spell_id,
                target_type: info.target_type,
                effects,
                mana_cost: info.mana_cost,
                cast_time_ms: info.cast_time_ms,
                recast_time_ms: info.recast_time_ms,
            });
        }
    }
    LegalActionMask { gems, abilities }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eqoxide_core::game_state::EMPTY_GEM;
    use eqoxide_core::spells::SpellInfo;

    fn db_with(id: u32, info: SpellInfo) -> SpellDb {
        let mut db = SpellDb::default();
        db.insert_for_test(id, info);
        db
    }

    #[test]
    fn empty_gems_produce_no_abilities_and_all_false_gems() {
        let mem_spells = [EMPTY_GEM; 9];
        let db = SpellDb::default();
        let mask = build_legal_actions(&mem_spells, &db);
        assert_eq!(mask.gems, [false; 9]);
        assert!(mask.abilities.is_empty());
    }

    #[test]
    fn a_memorized_spell_in_the_db_produces_a_true_gem_and_an_ability_with_blank_effects_filtered() {
        let mut mem_spells = [EMPTY_GEM; 9];
        mem_spells[2] = 12;
        let db = db_with(
            12,
            SpellInfo {
                name: "Minor Healing".into(),
                icon_id: 1,
                good_effect: 1,
                target_type: 5,
                effects: [79, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK, SPA_BLANK],
                mana_cost: 25,
                cast_time_ms: 3000,
                recast_time_ms: 0,
            },
        );
        let mask = build_legal_actions(&mem_spells, &db);
        assert!(mask.gems[2], "gem 2 holds a real spell id, so it must be legal");
        assert!(!mask.gems[0] && !mask.gems[1], "empty gems must stay false");
        assert_eq!(mask.abilities.len(), 1);
        assert_eq!(mask.abilities[0].gem, 2);
        assert_eq!(mask.abilities[0].spell_id, 12);
        assert_eq!(mask.abilities[0].target_type, 5);
        assert_eq!(mask.abilities[0].effects, vec![79], "SPA_BLANK slots must be filtered out");
        assert_eq!(mask.abilities[0].mana_cost, 25, "#1127: mana_cost must pass through from SpellInfo");
        assert_eq!(mask.abilities[0].cast_time_ms, 3000, "#1127: cast_time_ms must pass through from SpellInfo");
        assert_eq!(mask.abilities[0].recast_time_ms, 0, "#1127: recast_time_ms must pass through from SpellInfo");
    }

    #[test]
    fn a_memorized_spell_not_in_the_db_still_marks_the_gem_legal_with_no_ability_feature() {
        let mut mem_spells = [EMPTY_GEM; 9];
        mem_spells[0] = 999;
        let db = SpellDb::default();
        let mask = build_legal_actions(&mem_spells, &db);
        assert!(mask.gems[0]);
        assert!(mask.abilities.is_empty(), "no SpellInfo means no ability feature, but the gem is still legal");
    }
}
