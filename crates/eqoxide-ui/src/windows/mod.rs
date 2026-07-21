//! One file per window; uniform signature `fn draw(ui, cx)`. Dispatch by
//! registry id. Keep window bodies dumb: read `cx.scene`, write `cx.acts`,
//! defer manager mutations through `cx.cmds`.

use super::registry as reg;
use super::UiCtx;

mod actions;
mod casting;
mod chat;
mod compass;
mod group;
mod help;
mod inventory;
mod loot;
mod map;
mod merchant;
mod npc_dialogue;
mod options;
mod pet;
mod player;
mod quest_journal;
mod selector;
mod skills;
mod spellbook;
mod spellgems;
mod target;
mod trainer;

pub fn draw(id: &str, ui: &mut egui::Ui, cx: &mut UiCtx) {
    match id {
        reg::SELECTOR => selector::draw(ui, cx),
        reg::PLAYER => player::draw(ui, cx),
        reg::TARGET => target::draw(ui, cx),
        reg::GROUP => group::draw(ui, cx),
        reg::CHAT => chat::draw(ui, cx),
        reg::INVENTORY => inventory::draw(ui, cx),
        reg::MERCHANT => merchant::draw(ui, cx),
        reg::LOOT => loot::draw(ui, cx),
        reg::SPELL_GEMS => spellgems::draw(ui, cx),
        reg::CASTING => casting::draw(ui, cx),
        reg::SPELLBOOK => spellbook::draw(ui, cx),
        reg::SKILLS => skills::draw(ui, cx),
        reg::TRAINER => trainer::draw(ui, cx),
        reg::PET => pet::draw(ui, cx),
        reg::QUEST_JOURNAL => quest_journal::draw(ui, cx),
        reg::NPC_DIALOGUE => npc_dialogue::draw(ui, cx),
        reg::ACTIONS => actions::draw(ui, cx),
        reg::MAP => map::draw(ui, cx),
        reg::COMPASS => compass::draw(ui, cx),
        reg::OPTIONS => options::draw(ui, cx),
        reg::HELP => help::draw(ui, cx),
        _ => {
            ui.label(format!("unknown window {id}"));
        }
    }
}
