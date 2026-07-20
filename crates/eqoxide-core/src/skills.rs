//! EQ skill id → display name (RoF2 skill enum, from EQEmu `common/skills.h`). Used by
//! `GET /v1/observe/skills` and the trainer API (eqoxide#99). Ids `0..=76` are the real skills;
//! the PlayerProfile `skills[]` array is 100 wide on the wire but only these carry meaning.

/// Skill display names, indexed by skill id (0 = 1H Blunt … 76 = Triple Attack).
pub const SKILL_NAMES: &[&str] = &[
    "1H Blunt", "1H Slashing", "2H Blunt", "2H Slashing", "Abjuration", "Alteration",
    "Apply Poison", "Archery", "Backstab", "Bind Wound", "Bash", "Block", "Brass Instruments",
    "Channeling", "Conjuration", "Defense", "Disarm", "Disarm Traps", "Divination", "Dodge",
    "Double Attack", "Dragon Punch", "Dual Wield", "Eagle Strike", "Evocation", "Feign Death",
    "Flying Kick", "Forage", "Hand to Hand", "Hide", "Kick", "Meditate", "Mend", "Offense",
    "Parry", "Pick Lock", "1H Piercing", "Riposte", "Round Kick", "Safe Fall", "Sense Heading",
    "Singing", "Sneak", "Specialize Abjuration", "Specialize Alteration", "Specialize Conjuration",
    "Specialize Divination", "Specialize Evocation", "Pick Pockets", "Stringed Instruments",
    "Swimming", "Throwing", "Tiger Claw", "Tracking", "Wind Instruments", "Fishing", "Make Poison",
    "Tinkering", "Research", "Alchemy", "Baking", "Tailoring", "Sense Traps", "Blacksmithing",
    "Fletching", "Brewing", "Alcohol Tolerance", "Begging", "Jewelry Making", "Pottery",
    "Percussion Instruments", "Intimidation", "Berserking", "Taunt", "Frenzy", "Remove Traps",
    "Triple Attack",
];

/// Number of real skills (ids `0..NUM_SKILLS`). The rest of the wire array is unused padding.
pub const NUM_SKILLS: usize = 77;

/// Display name for a skill id, or `None` if the id is out of the known range.
pub fn skill_name(id: u32) -> Option<&'static str> {
    SKILL_NAMES.get(id as usize).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_covers_every_skill_id() {
        assert_eq!(SKILL_NAMES.len(), NUM_SKILLS);
        assert_eq!(skill_name(0), Some("1H Blunt"));
        assert_eq!(skill_name(7), Some("Archery"));
        assert_eq!(skill_name(31), Some("Meditate"));
        assert_eq!(skill_name(76), Some("Triple Attack"));
        assert_eq!(skill_name(77), None); // past the last real skill
        assert_eq!(skill_name(99), None); // wire padding
    }
}
