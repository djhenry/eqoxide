//! `eqoxide-ui` — the egui window system (issue #162).
//!
//! One registry of windows ([`registry`]), chromed + persisted per character
//! ([`chrome`], [`persist`]), themed after the native RoF2 client ([`theme`]),
//! with the Window Selector as the always-available control panel.
//!
//! Windows are dumb views: they read the per-frame [`SceneState`] snapshot and
//! write user actions into the same shared request slots the HTTP API uses
//! ([`Actions`]).
//!
//! Extracted as its own workspace crate (#544 Step 2o). It is the View half of the app's egui
//! surface: it depends on `eqoxide-core` (game_state/spells/skills/config/zone_map/pet),
//! `eqoxide-ipc` (the request-slot types `Actions` writes into, plus the `enabled` profiling
//! toggle), `eqoxide-command` (`CommandState`, the typed write-path facade), and `eqoxide-renderer`
//! (`SceneState`/`Billboard`, the per-frame render snapshot windows read) — plus egui/image/
//! shellexpand/serde/tracing. It has ZERO up-refs into the app crate (never `app`/`movement`/
//! `model`/`eq_net`/`http`). The app crate re-exports this crate as its `ui` module
//! (`pub use eqoxide_ui as ui;`), so every existing `crate::ui::…` / `eqoxide::ui::…` call site
//! (app.rs, main.rs, hud.rs) keeps resolving unchanged.

pub mod chrome;
pub mod icons;
pub mod persist;
pub mod registry;
pub mod theme;
pub mod widgets;
pub mod windows;

use eqoxide_renderer::scene::SceneState;
use eqoxide_core::zone_map::ZoneMap;
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
    /// The typed write-path facade (#446, #459). Combat/nav/camera/lifecycle write via
    /// `cx.acts.command.request_*` — no direct slot fields for those domains any more; the rest
    /// still use the raw slot fields below until they're migrated. See `eqoxide_command`.
    pub command: eqoxide_command::CommandState,
    pub hail: eqoxide_ipc::HailReq,
    pub say: eqoxide_ipc::SayReq,
    pub chat_send: eqoxide_ipc::ChatSendShared,
    pub dialogue_click: eqoxide_ipc::DialogueClickReq,
    pub sit: eqoxide_ipc::SitReq,
    pub move_item: eqoxide_ipc::MoveReq,
    pub loot: eqoxide_ipc::LootReq,
    pub accept_task: eqoxide_ipc::AcceptTaskReq,
    pub cancel_task: eqoxide_ipc::CancelTaskReq,
    pub group_invite: eqoxide_ipc::GroupInviteReq,
    pub group_accept: eqoxide_ipc::GroupAcceptReq,
    pub group_decline: eqoxide_ipc::GroupDeclineReq,
    pub group_leave: eqoxide_ipc::GroupLeaveReq,
    pub group_kick: eqoxide_ipc::GroupKickReq,
    pub group_make_leader: eqoxide_ipc::GroupMakeLeaderReq,
    /// Published camp deadline (read-path) for the HUD Camp button's countdown/toggle display.
    /// The camp REQUEST itself (and the HUD death-overlay Respawn button) route through
    /// `command.request_camp`/`request_respawn` (#459).
    pub camp_until: eqoxide_ipc::CampUntil,
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
    pub spells: &'a eqoxide_core::spells::SpellDb,
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
    /// Transient windows the user dismissed with the ✕ this session; cleared
    /// when the window's game-state gate goes false, so the next session
    /// (merchant visit, NPC reply, …) reopens it.
    dismissed: std::collections::HashSet<&'static str>,
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
            dismissed: std::collections::HashSet::new(),
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
    /// `screen_pts` is the TRUE point-space screen size computed by the caller
    /// — `ctx.screen_rect()` is wrong on the first frame after a zoom change.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_all(
        &mut self,
        ctx: &egui::Context,
        screen_pts: [f32; 2],
        scene: &SceneState,
        spells: &eqoxide_core::spells::SpellDb,
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
        // Clear LAST frame's reset flags before drawing, so a reset issued
        // during frame N is actually observed by chrome during frame N+1.
        self.sys.layout.end_frame();
        self.sys.layout.remap_all(screen_pts);
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, screen_pts.into());

        // Keep /r working: remember the sender of the most recent incoming tell
        // (logged as kind "tell" with a "<Sender> text" prefix).
        if let Some(sender) = scene
            .messages
            .iter()
            .rev()
            .find(|m| m.kind == "tell")
            .and_then(|m| m.text.strip_prefix('<'))
            .and_then(|t| t.split('>').next())
        {
            if !sender.is_empty() && !sender.eq_ignore_ascii_case(&scene.player_name) {
                self.chat.reply_to = sender.to_string();
            }
        }

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
                let gate = Self::transient_open(def.id, scene, acts);
                if !gate {
                    // Gate closed: forget any dismissal so the next session
                    // (new merchant visit, new NPC reply…) reopens the window.
                    self.dismissed.remove(def.id);
                }
                gate && !self.dismissed.contains(def.id)
            } else {
                // A pending group invite must be answerable even when the
                // Group window is closed: force it open while one is up.
                (def.id == registry::GROUP && scene.pending_invite.is_some())
                    || self.sys.layout.is_open(def.id, def.default_open)
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
            let win_t0 = eqoxide_ipc::enabled().then(std::time::Instant::now);
            let result = chrome::eq_window(ctx, &mut self.sys, def, screen, |ui| {
                windows::draw(def.id, ui, &mut cx)
            });
            if let Some(t0) = win_t0 {
                let ms = t0.elapsed().as_secs_f32() * 1000.0;
                if ms > 2.0 {
                    tracing::info!("ui profile: window '{}' took {ms:.1} ms", def.id);
                }
            }
            if result.close_clicked {
                if def.transient {
                    // Hide immediately; also tell the game to end the session
                    // where a protocol path exists.
                    self.dismissed.insert(def.id);
                    match def.id {
                        registry::MERCHANT => {
                            acts.command.request_merchant_trade(eqoxide_ipc::TradeCmd::Close);
                        }
                        // Some(0) = end-training sentinel (see navigation.rs).
                        registry::TRAINER => {
                            acts.command.request_open_trainer(0);
                        }
                        _ => {}
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

        self.sys.layout.maybe_save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actions() -> Actions {
        use std::sync::{Arc, Mutex};
        Actions {
            command: eqoxide_command::CommandState::default(),
            hail: Arc::new(Mutex::new(None)),
            say: Arc::new(Mutex::new(None)),
            chat_send: Arc::new(Mutex::new(Vec::new())),
            dialogue_click: Arc::new(Mutex::new(None)),
            sit: Arc::new(Mutex::new(None)),
            move_item: Arc::new(Mutex::new(None)),
            loot: Arc::new(Mutex::new(None)),
            accept_task: Arc::new(Mutex::new(None)),
            cancel_task: Arc::new(Mutex::new(None)),
            group_invite: Arc::new(Mutex::new(None)),
            group_accept: Arc::new(Mutex::new(None)),
            group_decline: Arc::new(Mutex::new(None)),
            group_leave: Arc::new(Mutex::new(None)),
            group_kick: Arc::new(Mutex::new(None)),
            group_make_leader: Arc::new(Mutex::new(None)),
            camp_until: Arc::new(Mutex::new(None)),
        }
    }

    /// Headless smoke test: every registered window draws without panicking,
    /// in both lock states, on an empty scene and again on a populated one.
    #[test]
    fn all_windows_draw_headless() {
        let mut ui = UiState::new("__uitest__", None);
        let acts = actions();
        let spells = eqoxide_core::spells::SpellDb::empty();
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
                ui.draw_all(ctx, [1280.0, 720.0], &scene, &spells, &acts, [0.0; 2], [100.0; 2], None, 60.0);
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
            ui.draw_all(ctx, [1280.0, 720.0], &scene, &spells, &acts, [0.0; 2], [100.0; 2], None, 60.0);
        });
        let _ = std::fs::remove_file(eqoxide_core::config::config_dir().join("ui_layout___uitest__.json"));
    }

    /// Regression: window sizes must STABILIZE across frames WITHIN ONE
    /// SESSION. A body that sizes its canvas from `available - <hardcoded
    /// footer>` and then draws a taller footer overflows its allotment; the
    /// window grows to fit, the body re-derives from the new size, and the
    /// window creeps across the screen forever while pinning the render loop
    /// (chat grew right, map grew down). Bodies must let bottom panels
    /// measure themselves.
    ///
    /// This does NOT cover the #613 bug (growth across a client *restart*,
    /// where the persisted "content size" silently included the title
    /// strip) — that needs a fresh `egui::Context` and a fresh persisted
    /// layout each cycle to simulate a real restart, which this in-session
    /// test never does. See `chrome::tests::round_trip_size_is_idempotent_across_reloads`.
    #[test]
    fn window_sizes_do_not_creep() {
        let mut ui = UiState::new("__uitest_growth__", None);
        let acts = actions();
        let spells = eqoxide_core::spells::SpellDb::empty();
        for def in REGISTRY {
            if !def.transient {
                ui.sys.layout.set_open(def.id, true);
            }
        }
        let mut scene = SceneState::default();
        scene.player_name = "Testy".into();
        scene.merchant_open = Some(42); // exercise the merchant transient too
        for i in 0..80 {
            scene.messages.push(eqoxide_core::game_state::LogEntry {
                kind: "chat".into(),
                text: format!("chatter line {i} with some width to it"),
                timestamp: std::time::Instant::now(),
                item_links: vec![],
            });
        }

        let ctx = egui::Context::default();
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            ..Default::default()
        };
        let frame = |ui: &mut UiState, ctx: &egui::Context| {
            let _ = ctx.run(raw.clone(), |ctx| {
                ui.draw_all(ctx, [1280.0, 720.0], &scene, &spells, &acts, [0.0; 2], [100.0; 2], None, 60.0);
            });
        };

        // Warm up (initial placement + first-frame sizing), then snapshot.
        for _ in 0..5 {
            frame(&mut ui, &ctx);
        }
        let snapshot: Vec<(&str, [f32; 2])> = REGISTRY
            .iter()
            .filter_map(|d| ui.sys.layout.observed(d.id).map(|(_, size)| (d.id, size)))
            .collect();
        assert!(!snapshot.is_empty(), "warmup produced no window rects");

        // 30 more frames: no window may keep growing.
        for _ in 0..30 {
            frame(&mut ui, &ctx);
        }
        for (id, before) in snapshot {
            let (_, after) = ui.sys.layout.observed(id).expect("window vanished");
            assert!(
                after[0] - before[0] < 3.0 && after[1] - before[1] < 3.0,
                "window '{id}' creeps: {before:?} -> {after:?} over 30 frames"
            );
        }
        let _ = std::fs::remove_file(
            eqoxide_core::config::config_dir().join("ui_layout___uitest_growth__.json"),
        );
    }

    #[test]
    fn hotkey_toggles_registered_window() {
        let mut ui = UiState::new("__uitest_hotkey__", None);
        assert!(!ui.layout().is_open(registry::INVENTORY, false));
        assert!(ui.hotkey(egui::Key::I));
        assert!(ui.layout().is_open(registry::INVENTORY, false));
        assert!(!ui.hotkey(egui::Key::F35));
        let _ = std::fs::remove_file(
            eqoxide_core::config::config_dir().join("ui_layout___uitest_hotkey__.json"),
        );
    }
}
