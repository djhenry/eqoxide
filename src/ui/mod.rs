//! The eqoxide window system (issue #162).
//!
//! One registry of windows ([`registry`]), chromed + persisted per character
//! ([`chrome`], [`persist`]), themed after the native RoF2 client ([`theme`]),
//! with the Window Selector as the always-available control panel.
//!
//! Windows are dumb views: they read the per-frame [`SceneState`] snapshot and
//! write user actions into the same shared request slots the HTTP API uses
//! ([`Actions`]).

pub mod chrome;
pub mod icons;
pub mod persist;
pub mod registry;
pub mod theme;
pub mod widgets;
pub mod windows;

use crate::scene::SceneState;
use crate::zone_map::ZoneMap;
use chrome::WinSys;
use registry::REGISTRY;

/// Design-space the UI zoom normalizes to (points). The zoom factor is
/// `ui_scale × min(w/REF_W, h/REF_H) / dpi` so the whole UI scales with the
/// window while text stays DPI-crisp.
pub const REF_W: f32 = 1280.0;
pub const REF_H: f32 = 720.0;

/// All the shared request slots UI windows can write. One clone lives on
/// `App`; the HTTP handlers and nav/gameplay threads hold the other ends.
#[derive(Clone)]
pub struct Actions {
    pub hail: crate::http::HailReq,
    pub say: crate::http::SayReq,
    pub chat_send: crate::http::ChatSendShared,
    pub dialogue_click: crate::http::DialogueClickReq,
    pub target: crate::http::TargetReq,
    pub attack: crate::http::AttackReq,
    pub cast: crate::http::CastReq,
    pub mem_spell: crate::http::MemSpellReq,
    pub sit: crate::http::SitReq,
    pub consider: crate::http::ConsiderReq,
    pub buy: crate::http::BuyReq,
    pub sell: crate::http::SellReq,
    pub trade: crate::http::TradeReq,
    pub move_item: crate::http::MoveReq,
    pub loot: crate::http::LootReq,
    pub accept_task: crate::http::AcceptTaskReq,
    pub cancel_task: crate::http::CancelTaskReq,
    pub trainer_open: crate::http::TrainerOpenReq,
    pub trainer_train: crate::http::TrainerTrainReq,
    pub group_invite: crate::http::GroupInviteReq,
    pub group_accept: crate::http::GroupAcceptReq,
    pub group_decline: crate::http::GroupDeclineReq,
    pub group_leave: crate::http::GroupLeaveReq,
    pub group_kick: crate::http::GroupKickReq,
    pub group_make_leader: crate::http::GroupMakeLeaderReq,
    pub camp: crate::http::CampReq,
    pub camp_until: crate::http::CampUntil,
}

/// Chat window runtime state (input buffer, active tab).
#[derive(Default)]
pub struct ChatState {
    pub input: String,
    pub tab: usize,
    /// Pre-filled recipient for /r (last incoming tell sender).
    pub reply_to: String,
}

/// Deferred command a window issues against the manager (applied after the
/// draw pass, since the layout is borrowed while windows draw).
pub enum UiCmd {
    Open(&'static str),
    Close(&'static str),
    Toggle(&'static str),
    SetUiScale(f32),
    SetLocked(bool),
    SetFades(bool),
    ResetAllWindows,
}

/// Everything a window body may read or write while drawing.
pub struct UiCtx<'a> {
    pub scene: &'a SceneState,
    pub spells: &'a crate::spells::SpellDb,
    pub icons: &'a mut icons::Icons,
    pub acts: &'a Actions,
    pub chat: &'a mut ChatState,
    pub cmds: &'a mut Vec<UiCmd>,
    // Read-only mirrors of manager state (mutations go through cmds).
    pub locked: bool,
    pub ui_scale: f32,
    pub fades: bool,
    /// (id, title, open, hotkey) for every non-transient window — the Selector list.
    pub window_list: &'a [(&'static str, &'static str, bool, Option<egui::Key>)],
    // Map data.
    pub zone_min: [f32; 2],
    pub zone_max: [f32; 2],
    pub zone_map: Option<&'a ZoneMap>,
    pub minimap_zoom: &'a mut f32,
    pub fps: f32,
}

/// Owns all UI runtime state on the render thread.
pub struct UiState {
    pub sys: WinSys,
    pub icons: icons::Icons,
    pub chat: ChatState,
    pub minimap_zoom: f32,
    cmds: Vec<UiCmd>,
    theme_applied: bool,
}

impl UiState {
    pub fn new(character_name: &str, icons_dir: Option<String>) -> Self {
        UiState {
            sys: WinSys::new(persist::Layout::load(character_name)),
            icons: icons::Icons::new(icons_dir),
            chat: ChatState::default(),
            minimap_zoom: 1.0,
            cmds: Vec::new(),
            theme_applied: false,
        }
    }

    pub fn layout(&self) -> &persist::Layout {
        &self.sys.layout
    }
    pub fn layout_mut(&mut self) -> &mut persist::Layout {
        &mut self.sys.layout
    }

    /// Toggle a window from a hotkey (no-op for unknown / transient ids).
    pub fn hotkey(&mut self, key: egui::Key) -> bool {
        for def in REGISTRY {
            if def.hotkey == Some(key) && !def.transient {
                self.sys.layout.toggle_open(def.id, def.default_open);
                return true;
            }
        }
        false
    }

    /// Is a transient window forced open/closed by game state this frame?
    fn transient_open(id: &str, scene: &SceneState, acts: &Actions) -> bool {
        match id {
            registry::MERCHANT => scene.merchant_open.is_some(),
            registry::TRAINER => scene.trainer_open.is_some(),
            registry::CASTING => scene.casting.is_some(),
            registry::LOOT => scene.loot_active,
            registry::NPC_DIALOGUE => {
                !scene.dialogue_choices.is_empty()
                    || scene
                        .messages
                        .iter()
                        .any(|m| m.kind == "npc" && m.timestamp.elapsed().as_secs() < 45)
            }
            _ => {
                let _ = acts;
                false
            }
        }
    }

    /// Draw every open window. Call once per frame inside `egui::Context::run`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_all(
        &mut self,
        ctx: &egui::Context,
        scene: &SceneState,
        spells: &crate::spells::SpellDb,
        acts: &Actions,
        zone_min: [f32; 2],
        zone_max: [f32; 2],
        zone_map: Option<&ZoneMap>,
        fps: f32,
    ) {
        if !self.theme_applied {
            theme::apply(ctx);
            self.theme_applied = true;
        }
        let sr = ctx.screen_rect();
        self.sys.layout.remap_all([sr.width(), sr.height()]);

        let window_list: Vec<(&'static str, &'static str, bool, Option<egui::Key>)> = REGISTRY
            .iter()
            .filter(|d| !d.transient && d.id != registry::SELECTOR)
            .map(|d| {
                (
                    d.id,
                    d.title,
                    self.sys.layout.is_open(d.id, d.default_open),
                    d.hotkey,
                )
            })
            .collect();

        let mut cmds = std::mem::take(&mut self.cmds);
        for def in REGISTRY {
            let open = if def.transient {
                Self::transient_open(def.id, scene, acts)
            } else {
                self.sys.layout.is_open(def.id, def.default_open)
            };
            if !open {
                continue;
            }
            let mut cx = UiCtx {
                scene,
                spells,
                icons: &mut self.icons,
                acts,
                chat: &mut self.chat,
                cmds: &mut cmds,
                locked: self.sys.layout.locked,
                ui_scale: self.sys.layout.ui_scale,
                fades: self.sys.layout.fades,
                window_list: &window_list,
                zone_min,
                zone_max,
                zone_map,
                minimap_zoom: &mut self.minimap_zoom,
                fps,
            };
            let result = chrome::eq_window(ctx, &mut self.sys, def, |ui| {
                windows::draw(def.id, ui, &mut cx)
            });
            if result.close_clicked {
                if def.transient {
                    // Transients close by telling the game to end the session.
                    if def.id == registry::MERCHANT {
                        *acts.trade.lock().unwrap() = Some(crate::http::TradeCmd::Close);
                    }
                } else {
                    self.sys.layout.set_open(def.id, false);
                }
            }
        }

        // Apply deferred window commands.
        for cmd in cmds.drain(..) {
            match cmd {
                UiCmd::Open(id) => self.sys.layout.set_open(id, true),
                UiCmd::Close(id) => self.sys.layout.set_open(id, false),
                UiCmd::Toggle(id) => {
                    let d = registry::get(id).map(|d| d.default_open).unwrap_or(false);
                    self.sys.layout.toggle_open(id, d);
                }
                UiCmd::SetUiScale(s) => self.sys.layout.set_ui_scale(s),
                UiCmd::SetLocked(l) => self.sys.layout.set_locked(l),
                UiCmd::SetFades(f) => self.sys.layout.set_fades(f),
                UiCmd::ResetAllWindows => self.sys.layout.reset_all(),
            }
        }
        self.cmds = cmds;

        self.sys.layout.end_frame();
        self.sys.layout.maybe_save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions() -> Actions {
        use std::sync::{Arc, Mutex};
        Actions {
            hail: Arc::new(Mutex::new(None)),
            say: Arc::new(Mutex::new(None)),
            chat_send: Arc::new(Mutex::new(Vec::new())),
            dialogue_click: Arc::new(Mutex::new(None)),
            target: Arc::new(Mutex::new(None)),
            attack: Arc::new(Mutex::new(None)),
            cast: Arc::new(Mutex::new(None)),
            mem_spell: Arc::new(Mutex::new(None)),
            sit: Arc::new(Mutex::new(None)),
            consider: Arc::new(Mutex::new(None)),
            buy: Arc::new(Mutex::new(None)),
            sell: Arc::new(Mutex::new(None)),
            trade: Arc::new(Mutex::new(None)),
            move_item: Arc::new(Mutex::new(None)),
            loot: Arc::new(Mutex::new(None)),
            accept_task: Arc::new(Mutex::new(None)),
            cancel_task: Arc::new(Mutex::new(None)),
            trainer_open: Arc::new(Mutex::new(None)),
            trainer_train: Arc::new(Mutex::new(None)),
            group_invite: Arc::new(Mutex::new(None)),
            group_accept: Arc::new(Mutex::new(None)),
            group_decline: Arc::new(Mutex::new(None)),
            group_leave: Arc::new(Mutex::new(None)),
            group_kick: Arc::new(Mutex::new(None)),
            group_make_leader: Arc::new(Mutex::new(None)),
            camp: Arc::new(Mutex::new(None)),
            camp_until: Arc::new(Mutex::new(None)),
        }
    }

    /// Headless smoke test: every registered window draws without panicking,
    /// in both lock states, on an empty scene and again on a populated one.
    #[test]
    fn all_windows_draw_headless() {
        let mut ui = UiState::new("__uitest__", None);
        let acts = actions();
        let spells = crate::spells::SpellDb::empty();
        // Force every non-transient window open.
        for def in REGISTRY {
            if !def.transient {
                ui.sys.layout.set_open(def.id, true);
            }
        }
        let mut scene = SceneState::default();
        for locked in [false, true] {
            ui.sys.layout.set_locked(locked);
            let ctx = egui::Context::default();
            let _ = ctx.run(Default::default(), |ctx| {
                ui.draw_all(ctx, &scene, &spells, &acts, [0.0; 2], [100.0; 2], None, 60.0);
            });
        }
        // Populated-ish scene (merchant open forces the transient path too).
        scene.player_name = "Testy".into();
        scene.player_level = 10;
        scene.player_hp_pct = 55.0;
        scene.merchant_open = Some(42);
        scene.coin = [1, 2, 3, 4];
        let ctx = egui::Context::default();
        let _ = ctx.run(Default::default(), |ctx| {
            ui.draw_all(ctx, &scene, &spells, &acts, [0.0; 2], [100.0; 2], None, 60.0);
        });
        let _ = std::fs::remove_file(crate::config::config_dir().join("ui_layout___uitest__.json"));
    }

    #[test]
    fn hotkey_toggles_registered_window() {
        let mut ui = UiState::new("__uitest_hotkey__", None);
        assert!(!ui.layout().is_open(registry::INVENTORY, false));
        assert!(ui.hotkey(egui::Key::I));
        assert!(ui.layout().is_open(registry::INVENTORY, false));
        assert!(!ui.hotkey(egui::Key::F35));
        let _ = std::fs::remove_file(
            crate::config::config_dir().join("ui_layout___uitest_hotkey__.json"),
        );
    }
}
