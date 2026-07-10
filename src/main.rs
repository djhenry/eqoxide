//! Full-client entry point.
//!
//! Loads config + the EQ string table, creates the shared request slots (`Arc<Mutex<…>>`) and the
//! mpsc packet channel, then starts the three concurrent halves: the EQ network thread
//! (`run_login_flow`, skipped with `--testzone`), the HTTP API server, and the winit/wgpu render
//! loop on the main thread. The request slots are the cross-thread glue — HTTP writes them, the nav
//! thread drains them. `--testzone` runs the renderer offline (no server) for asset/zone debugging.

use eqoxide::{assets, camera_state, config, eq_net, eqstr, http};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use winit::event_loop::EventLoop;

fn main() {
    eqoxide::logging::init();

    // Parse + STRICTLY validate CLI args. We error out (with help) on anything malformed or
    // unrecognized rather than silently falling back to defaults — a silent fallback once made the
    // client log into the wrong account when --config pointed at a missing file.
    const USAGE: &str = "\
eqoxide — EverQuest (Titanium) client

USAGE:
    eqoxide [OPTIONS]

OPTIONS:
    --config <name|path>   Per-character login config. A profile name resolves to
                           ~/.config/eqoxide/config-<name>.yaml; a *.yaml/*.yml filename resolves
                           under ~/.config/eqoxide/; a value with a '/' is used as a literal path.
                           Omit to use the default ~/.config/eqoxide/config.yaml.
    --testzone             Run the renderer offline (no server) for asset/zone debugging.
    --profile              Enable the per-phase frame-timing HUD overlay.
    --nav-debug            Show the navmesh/pathfinding debug overlay at startup (collision floor
                           grid + live A* path to the current goal). Toggle at runtime with F11.
    --api-port <N>         Bind the agent HTTP API to exactly TCP port N (1-65535), instead of
                           scanning upward from the config base port. The launch's API is
                           disabled if N is already in use. Use a port you've reserved via a
                           /tmp lockfile so concurrent test clients don't collide.
    -h, --help             Show this help and exit.
";
    let args: Vec<String> = std::env::args().collect();
    let mut testzone_mode = false;
    let mut profile_flag  = false;
    let mut nav_debug_flag = false;
    let mut login_cfg_arg: Option<String> = None;
    let mut api_port_arg: Option<u16> = None;
    let mut idx = 1; // skip argv[0] (program name)
    while idx < args.len() {
        let arg = args[idx].as_str();
        match arg {
            "--testzone" => testzone_mode = true,
            "--profile"  => profile_flag  = true,
            "--nav-debug" => nav_debug_flag = true,
            "-h" | "--help" => { print!("{USAGE}"); std::process::exit(0); }
            // accept both "--config <value>" and "--config=<value>"
            _ if arg == "--config" || arg.starts_with("--config=") => {
                let value = if let Some(v) = arg.strip_prefix("--config=") {
                    v.to_string()
                } else {
                    match args.get(idx + 1) {
                        Some(v) if !v.starts_with('-') => { idx += 1; v.clone() }
                        _ => {
                            eprintln!("error: --config requires a value (a profile name or config file path)\n\n{USAGE}");
                            std::process::exit(2);
                        }
                    }
                };
                if value.is_empty() {
                    eprintln!("error: --config requires a non-empty value\n\n{USAGE}");
                    std::process::exit(2);
                }
                login_cfg_arg = Some(value);
            }
            // accept both "--api-port <value>" and "--api-port=<value>"
            _ if arg == "--api-port" || arg.starts_with("--api-port=") => {
                let value = if let Some(v) = arg.strip_prefix("--api-port=") {
                    v.to_string()
                } else {
                    match args.get(idx + 1) {
                        Some(v) if !v.starts_with('-') => { idx += 1; v.clone() }
                        _ => {
                            eprintln!("error: --api-port requires a value (a TCP port 1-65535)\n\n{USAGE}");
                            std::process::exit(2);
                        }
                    }
                };
                match value.parse::<u16>() {
                    Ok(p) if p > 0 => api_port_arg = Some(p),
                    _ => {
                        eprintln!("error: --api-port must be a number 1-65535, got '{value}'\n\n{USAGE}");
                        std::process::exit(2);
                    }
                }
            }
            other => {
                eprintln!("error: unrecognized argument '{other}'\n\n{USAGE}");
                std::process::exit(2);
            }
        }
        idx += 1;
    }

    // `--profile` (or EQ_PROFILE=1) enables the lightweight per-phase frame-timing HUD overlay.
    let profile_mode = profile_flag
        || std::env::var("EQ_PROFILE").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
    eqoxide::profiling::set_enabled(profile_mode);

    // Resolve the login config. When --config is given the resolved file MUST exist — we never fall
    // back to the default config in that case. The default ~/.config/eqoxide/config.yaml is used
    // only when --config is omitted.
    let login_cfg_path = config::LoginConfig::resolve_path(login_cfg_arg.as_deref());
    if login_cfg_arg.is_some() && !login_cfg_path.exists() {
        eprintln!("error: config file not found for --config {}: {}\n\n{USAGE}",
            login_cfg_arg.as_deref().unwrap_or(""), login_cfg_path.display());
        std::process::exit(2);
    }
    tracing::info!("renderer: loading login config from {}", login_cfg_path.display());

    let login_cfg = config::LoginConfig::load(&login_cfg_path);
    let app_cfg   = config::AppConfig::load();

    // Game data (string table, spell DB, zone maps + water regions) is delivered by the asset
    // server's "gamedata" set and synced into the local cache — NOT read from ~/eq_assets. This
    // removes the runtime dependency on the original game content for these files. Synced early
    // (before the loads below) and best-effort: a failure logs a warning and the affected features
    // degrade rather than aborting. (--testzone is offline, so skip the sync there.)
    let cache = eqoxide::asset_sync::CacheDirs::resolve();
    let data_dir = cache.models_dir();
    if !testzone_mode {
        match eqoxide::asset_sync::AssetSync::login(
            &app_cfg.asset_server_url, &login_cfg.username, &login_cfg.password)
        {
            Ok(sync) => {
                // gamedata = string table / spells / maps; gameequip = worn-armor texture + held-
                // weapon S3D archives. Both land in the cache so nothing is read from ~/eq_assets.
                for set in ["gamedata", "gameequip"] {
                    if let Err(e) = eqoxide::asset_sync::sync_set(&sync, set, &cache, &mut |_| {}) {
                        tracing::warn!("{set} sync failed: {e} — related assets may be unavailable");
                    }
                }
            }
            Err(e) => tracing::warn!("asset server login failed: {e} — game data/equip not synced"),
        }
    }

    // Load the EQ string table for OP_FormattedMessage / OP_SimpleMessage rendering.
    eqstr::load(&data_dir.join("eqstr_us.txt"));

    // Load quest-giver data for the golden "!" indicators and the GET /quests endpoint. Delivered
    // through the asset server's "gamedata" set (synced into the cache above), so a server can ship
    // custom quests as content — not bundled with the client. Missing file (e.g. --testzone, which
    // skips the sync) = no indicators, client still runs.
    eqoxide::quests::load(&data_dir.join("quests.json"));

    let camera_cmd: Arc<Mutex<Option<camera_state::CameraCmd>>> = Arc::new(Mutex::new(None));
    let camera_snapshot: Arc<Mutex<camera_state::CameraSnapshot>> = Arc::new(Mutex::new(
        camera_state::CameraState::new([0.0, 0.0, 0.0], 0.0).snapshot(),
    ));

    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<eq_net::AppPacket>();

    // Shared clean-shutdown flag. Set by window-close, a completed camp, and signals; observed by
    // the EQ network thread, which performs the logout sequence and exits the process.
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

    // Camp slots. `camp` carries a pending camp command (/exit, /camp, HUD button, `/camp` chat);
    // `camp_until` is the published camp deadline (Some while camping) for the HUD countdown.
    let camp:       http::CampReq   = Arc::new(Mutex::new(None));
    let camp_until: http::CampUntil = Arc::new(Mutex::new(None));

    // Route SIGTERM/SIGINT into the same clean-shutdown flag so a killed process (e.g.
    // `timeout N ./eqoxide`, Ctrl-C, or `kill <pid>`) logs out cleanly instead of dropping
    // its UDP stream. A sudden drop leaves the character LINKDEAD on the server for
    // Zone:ClientLinkdeadMS (90s) before it can be re-logged; a clean OP_Logout removes it
    // immediately. signal-hook's handler only stores into the AtomicBool (async-signal-safe);
    // the network thread observes the flag and runs the OP_Logout sequence.
    for sig in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        if let Err(e) = signal_hook::flag::register(sig, Arc::clone(&shutdown)) {
            tracing::warn!("warning: failed to register signal {sig} for clean shutdown: {e}");
        }
    }

    let goto_target:      http::GotoTarget      = Arc::new(Mutex::new(None));
    let goto_entity:      http::GotoEntity      = Arc::new(Mutex::new(None));
    let entity_positions: http::EntityPositions = Arc::new(Mutex::new(HashMap::new()));
    let entity_ids:       http::EntityIds       = Arc::new(Mutex::new(HashMap::new()));
    let zone_points:      http::ZonePoints      = Arc::new(Mutex::new(Vec::new()));
    let task_log:         http::TaskLog         = Arc::new(Mutex::new(Vec::new()));
    let task_offers_shared:    http::TaskOffersShared    = Arc::new(Mutex::new(Vec::new()));
    let completed_tasks_shared: http::CompletedTasksShared = Arc::new(Mutex::new(Vec::new()));
    let accept_task:           http::AcceptTaskReq        = Arc::new(Mutex::new(None));
    let cancel_task:           http::CancelTaskReq        = Arc::new(Mutex::new(None));
    let group:             http::GroupShared         = Arc::new(Mutex::new(http::GroupSnapshot::default()));
    let group_invite:      http::GroupInviteReq      = Arc::new(Mutex::new(None));
    let trainer_open_req:  http::TrainerOpenReq      = Arc::new(Mutex::new(None));
    let trainer_train_req: http::TrainerTrainReq     = Arc::new(Mutex::new(None));
    let group_accept:      http::GroupAcceptReq      = Arc::new(Mutex::new(None));
    let group_decline:     http::GroupDeclineReq     = Arc::new(Mutex::new(None));
    let group_leave:       http::GroupLeaveReq       = Arc::new(Mutex::new(None));
    let group_kick:        http::GroupKickReq        = Arc::new(Mutex::new(None));
    let group_make_leader: http::GroupMakeLeaderReq  = Arc::new(Mutex::new(None));
    let zone_cross:       http::ZoneCrossReq    = Arc::new(Mutex::new(None));
    let manual_move:      http::ManualMoveReq   = Arc::new(Mutex::new(None));
    let hail:             http::HailReq         = Arc::new(Mutex::new(None));
    let say:              http::SayReq          = Arc::new(Mutex::new(None));
    let target:           http::TargetReq       = Arc::new(Mutex::new(None));
    let attack:           http::AttackReq       = Arc::new(Mutex::new(None));
    let buy:              http::BuyReq          = Arc::new(Mutex::new(None));
    let sell:             http::SellReq         = Arc::new(Mutex::new(None));
    let trade:            http::TradeReq        = Arc::new(Mutex::new(None));
    let merchant:         http::MerchantShared  = Arc::new(Mutex::new(http::MerchantSnapshot::default()));
    let move_req:         http::MoveReq         = Arc::new(Mutex::new(None));
    let give:             http::GiveReq         = Arc::new(Mutex::new(None));
    let inventory:        http::InventoryShared = Arc::new(Mutex::new(Vec::new()));
    let loot:             http::LootReq         = Arc::new(Mutex::new(None));
    let door_click:       http::DoorClickReq    = Arc::new(Mutex::new(None));
    let doors_shared:     http::DoorsShared     = Arc::new(Mutex::new(Vec::new()));
    let messages:         http::MessagesShared  = Arc::new(Mutex::new(Vec::new()));
    let dialogue:         http::DialogueShared   = Arc::new(Mutex::new(Vec::new()));
    let nav_state:        http::NavStateShared   = Arc::new(Mutex::new("idle".to_string()));
    let dialogue_click:   http::DialogueClickReq = Arc::new(Mutex::new(None));
    let chat_events:      http::ChatEventsShared = Arc::new(Mutex::new(Vec::new()));
    let chat_send:        http::ChatSendShared   = Arc::new(Mutex::new(Vec::new()));
    let cast:             http::CastReq         = Arc::new(Mutex::new(None));
    let mem_spell:        http::MemSpellReq     = Arc::new(Mutex::new(None));
    let sit:              http::SitReq          = Arc::new(Mutex::new(None));
    let consider:         http::ConsiderReq     = Arc::new(Mutex::new(None));
    let pet_cmd:          http::PetCmdReq       = Arc::new(Mutex::new(None));
    // spells_us.txt is an EQ data file; default to the configured assets dir,
    // overridable via EQ_SPELLS_FILE.
    let spells_path = std::env::var("EQ_SPELLS_FILE")
        .unwrap_or_else(|_| data_dir.join("spells_us.txt").to_string_lossy().into_owned());
    let spells: std::sync::Arc<eqoxide::spells::SpellDb> =
        std::sync::Arc::new(eqoxide::spells::SpellDb::load(&spells_path));
    // Publish globally so the nav thread can resolve spell target types for self-cast (eqoxide#95).
    eqoxide::spells::set_global(spells.clone());
    let shared_collision: assets::SharedCollision = Arc::new(std::sync::RwLock::new(None));
    let frame_req:        http::FrameReq        = Arc::new(Mutex::new(None));
    let player_info:      http::PlayerInfo      = Arc::new(Mutex::new(http::PlayerState::default()));
    // Single-authority movement (Component A): the render thread owns the CharacterController and
    // publishes `controller_view`; the nav thread streams it and writes `nav_intent` for /goto;
    // `pos_correction` hands a server correction back to the controller.
    let controller_view:  http::ControllerShared = Arc::new(Mutex::new(eqoxide::movement::ControllerView::default()));
    let nav_intent:       http::NavIntent        = Arc::new(Mutex::new(None));
    let pos_correction:   http::PosCorrection     = Arc::new(Mutex::new(None));
    // Walker's live plan, published by the nav thread and drawn by the nav-debug overlay (#246).
    let nav_path_view:    http::NavPathView       = Arc::new(Mutex::new((Vec::new(), Vec::new())));

    // EQ network task — skipped in --testzone mode (offline debug)
    let character_name = login_cfg.character_name.clone();
    let asset_user     = login_cfg.username.clone();
    let asset_pass     = login_cfg.password.clone();
    let asset_server_url = app_cfg.asset_server_url.clone();
    if !testzone_mode {
        let gt  = goto_target.clone();
        let ge  = goto_entity.clone();
        let ep  = entity_positions.clone();
        let ei  = entity_ids.clone();
        let zp  = zone_points.clone();
        let tl  = task_log.clone();
        let tos = task_offers_shared.clone();
        let cts = completed_tasks_shared.clone();
        let atk = accept_task.clone();
        let ctk = cancel_task.clone();
        let gr  = group.clone();
        let gi  = group_invite.clone();
        let tor = trainer_open_req.clone();
        let ttr = trainer_train_req.clone();
        let ga  = group_accept.clone();
        let gd  = group_decline.clone();
        let gl  = group_leave.clone();
        let gk  = group_kick.clone();
        let gml = group_make_leader.clone();
        let zc  = zone_cross.clone();
        let hl  = hail.clone();
        let sy  = say.clone();
        let tg  = target.clone();
        let at  = attack.clone();
        let by  = buy.clone();
        let sl  = sell.clone();
        let tr  = trade.clone();
        let mc  = merchant.clone();
        let mv  = move_req.clone();
        let gv  = give.clone();
        let iv  = inventory.clone();
        let lt  = loot.clone();
        let dc  = door_click.clone();
        let ds  = doors_shared.clone();
        let mg  = messages.clone();
        let dlg = dialogue.clone();
        let dcl = dialogue_click.clone();
        let nst = nav_state.clone();
        let cev = chat_events.clone();
        let csd = chat_send.clone();
        let ca  = cast.clone();
        let ms  = mem_spell.clone();
        let st  = sit.clone();
        let co  = consider.clone();
        let pcm = pet_cmd.clone();
        let sc  = shared_collision.clone();
        let sd  = shutdown.clone();
        let cp  = camp.clone();
        let cu  = camp_until.clone();
        let cv  = controller_view.clone();
        let ni  = nav_intent.clone();
        let pc  = pos_correction.clone();
        let npv = nav_path_view.clone();
        let md  = data_dir.join("maps");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                if let Err(e) = eq_net::run_login_flow(login_cfg, app_tx, 10, gt, nst, ge, ep, ei, zp, tl, tos, cts, atk, ctk, gr, gi, tor, ttr, ga, gd, gl, gk, gml, zc, hl, sy, tg, at, by, sl, tr, mc, mv, gv, iv, lt, dc, ds, mg, dlg, dcl, cev, csd, ca, ms, st, co, pcm, sc, md, sd, cp, cu, cv, ni, pc, npv).await {
                    tracing::error!("EQ: fatal: {e}");
                }
            });
        });
    }

    // HTTP server
    let app_goto = goto_target.clone();
    // All the request slots UI windows can write, bundled (#162). These are the
    // same slots the HTTP API and nav/gameplay threads share.
    let app_actions = eqoxide::ui::Actions {
        hail: hail.clone(),
        say: say.clone(),
        chat_send: chat_send.clone(),
        dialogue_click: dialogue_click.clone(),
        target: target.clone(),
        attack: attack.clone(),
        cast: cast.clone(),
        mem_spell: mem_spell.clone(),
        sit: sit.clone(),
        consider: consider.clone(),
        buy: buy.clone(),
        sell: sell.clone(),
        trade: trade.clone(),
        move_item: move_req.clone(),
        loot: loot.clone(),
        accept_task: accept_task.clone(),
        cancel_task: cancel_task.clone(),
        trainer_open: trainer_open_req.clone(),
        trainer_train: trainer_train_req.clone(),
        group_invite: group_invite.clone(),
        group_accept: group_accept.clone(),
        group_decline: group_decline.clone(),
        group_leave: group_leave.clone(),
        group_kick: group_kick.clone(),
        group_make_leader: group_make_leader.clone(),
        camp: camp.clone(),
        camp_until: camp_until.clone(),
        pet_cmd: pet_cmd.clone(),
    };
    let app_spells  = spells.clone();
    let app_door_click = door_click.clone();
    let app_player_info = player_info.clone();
    // --api-port N: bind exactly N now and FAIL THE LAUNCH if it's taken (don't open a window with
    // a dead API). The bound listener is handed to the server thread so there's no re-bind race.
    // Without --api-port, pass None and let the server scan upward from the config base port.
    let exact_listener: Option<std::net::TcpListener> = match api_port_arg {
        Some(p) => match std::net::TcpListener::bind(("127.0.0.1", p)) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("error: --api-port {p} is unavailable ({e}). Free the port or choose another.");
                std::process::exit(1);
            }
        },
        None => None,
    };
    http::spawn_camera_server(
        camera_cmd.clone(),
        camera_snapshot.clone(),
        frame_req.clone(),
        goto_target,
        goto_entity,
        entity_positions,
        entity_ids,
        zone_points,
        shared_collision.clone(),
        zone_cross,
        manual_move.clone(),
        hail,
        say,
        target,
        attack,
        cast.clone(),
        mem_spell.clone(),
        sit.clone(),
        consider.clone(),
        buy,
        sell,
        trade,
        merchant,
        move_req,
        give,
        inventory,
        loot,
        messages,
        dialogue,
        nav_state,
        dialogue_click,
        chat_events,
        chat_send,
        spells.clone(),
        player_info,
        task_log,
        task_offers_shared,
        completed_tasks_shared,
        accept_task,
        cancel_task,
        group,
        group_invite,
        trainer_open_req,
        trainer_train_req,
        group_accept,
        group_decline,
        group_leave,
        group_kick,
        group_make_leader,
        door_click,
        doors_shared,
        camp.clone(),
        camp_until.clone(),
        pet_cmd.clone(),
        app_cfg.http_port,
        exact_listener,
    );

    let event_loop = EventLoop::new().expect("event loop");
    let mut application = eqoxide::app::App::new(
        app_cfg.assets_path,
        app_cfg.models_path,
        character_name,
        camera_cmd,
        camera_snapshot,
        app_rx,
        frame_req,
        app_goto,
        app_actions,
        app_spells,
        app_door_click,
        shared_collision,
        app_player_info,
        testzone_mode,
        nav_debug_flag,
        shutdown.clone(),
        app_cfg.eq_ui_dir,
        asset_server_url,
        asset_user,
        asset_pass,
        controller_view,
        nav_intent,
        manual_move,
        pos_correction,
        nav_path_view,
    );
    event_loop.run_app(&mut application).expect("event loop run");
    // The event loop has now exited gracefully — either the window was closed, or a shutdown was
    // requested (POST /exit / OP_GMKick set the flag and `about_to_wait` called `event_loop.exit()`).
    // Either way winit has torn down its Wayland clipboard worker on this (main) thread, so it is now
    // safe to exit the process. Ensure the flag is set so the EQ network thread logs the character
    // out (it idles after sending OP_Logout + OP_SessionDisconnect), give it a moment, then exit.
    shutdown.store(true, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    std::process::exit(0);
}
