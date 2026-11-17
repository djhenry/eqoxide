//! Refusal rules that a verb and an op both apply, so each has one definition.

/// The highest valid spell gem index. `GameState::mem_spells` is a fixed array of nine gems.
pub const MAX_GEM: u32 = 8;

/// Returns the refusal for a `gem` outside `0..=MAX_GEM`, or `None` when `gem` is valid.
pub fn gem_refusal(gem: u32) -> Option<String> {
    (gem > MAX_GEM).then(|| format!("gem {gem} is out of range; gems are 0-{MAX_GEM}"))
}

/// Returns the refusal for casting from a `gem` that contains no spell, or `None` when `gem`
/// contains a spell or lies outside the nine gems.
pub fn empty_gem_refusal(gem: u32, mem_spells: &[u32]) -> Option<String> {
    let spell_id = *mem_spells.get(gem as usize)?;
    eqoxide_core::game_state::gem_is_empty(spell_id)
        .then(|| format!("spell gem {gem} is empty; memorize a spell into it first"))
}

/// Returns the refusal for a request made while the network thread is not running, where `reason`
/// is the published reason and `subject` names what was refused (`"The verb"`, `"This command"`).
pub fn net_thread_not_running(reason: &str, subject: &str) -> String {
    format!(
        "the network thread is not running: {reason} {subject} was NOT sent and will not take \
         effect. Do not retry: nothing will drain it."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_gem_past_the_ninth_is_refused() {
        assert_eq!(gem_refusal(0), None);
        assert_eq!(gem_refusal(MAX_GEM), None);
        assert!(gem_refusal(MAX_GEM + 1).unwrap().contains("0-8"));
    }

    #[test]
    fn a_gem_without_a_spell_is_refused() {
        let gems = [200, eqoxide_core::game_state::EMPTY_GEM, 0];
        assert_eq!(empty_gem_refusal(0, &gems), None);
        assert!(empty_gem_refusal(1, &gems).unwrap().contains("gem 1 is empty"));
        assert!(empty_gem_refusal(2, &gems).is_some());
        assert_eq!(empty_gem_refusal(9, &gems), None);
    }

    #[test]
    fn the_net_thread_refusal_names_the_reason_and_the_subject() {
        let msg = net_thread_not_running("it panicked.", "The verb");
        assert!(msg.contains("it panicked."), "{msg}");
        assert!(msg.contains("The verb was NOT sent"), "{msg}");
    }
}
