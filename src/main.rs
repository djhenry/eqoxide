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
    -h, --help             Show this help and exit.
";
    let args: Vec<String> = std::env::args().collect();
    let mut testzone_mode = false;
    let mut profile_flag  = false;
    let mut login_cfg_arg: Option<String> = None;
    let mut idx = 1; // skip argv[0] (program name)
    while idx < args.len() {
        let arg = args[idx].as_str();
        match arg {
            "--testzone" => testzone_mode = true,
            "--profile"  => profile_flag  = true,
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

    // Load quest-giver data (generated by tools/quest_finder.py --export) for the golden "!"
    // indicators and the GET /quests endpoint. Missing file = no indicators (client still runs).
    eqoxide::quests::load(std::path::Path::new("data/quests.json"));

    let camera_cmd: Arc<Mutex<Option<camera_state::CameraCmd>>> = Arc::new(Mutex::new(None));
    let camera_snapshot: Arc<Mutex<camera_state::CameraSnapshot>> = Arc::new(Mutex::new(
        camera_state::CameraState::new([0.0, 0.0, 0.0], 0.0).snapshot(),
    ));

    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<eq_net::AppPacket>();

    // Shared clean-shutdown flag. Set by POST /exit and by window-close; observed by the
    // EQ network thread, which performs the logout sequence and exits the process.
    let shutdown: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));

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
    let entity_positions: http::EntityPositions = Arc::new(Mutex::new(HashMap::new()));
    let entity_ids:       http::EntityIds       = Arc::new(Mutex::new(HashMap::new()));
    let zone_points:      http::ZonePoints      = Arc::new(Mutex::new(Vec::new()));
    let task_log:         http::TaskLog         = Arc::new(Mutex::new(Vec::new()));
    let zone_cross:       http::ZoneCrossReq    = Arc::new(Mutex::new(None));
    let warp:             http::WarpReq         = Arc::new(Mutex::new(None));
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
    let cast:             http::CastReq         = Arc::new(Mutex::new(None));
    let mem_spell:        http::MemSpellReq     = Arc::new(Mutex::new(None));
    let sit:              http::SitReq          = Arc::new(Mutex::new(None));
    let consider:         http::ConsiderReq     = Arc::new(Mutex::new(None));
    // spells_us.txt is an EQ data file; default to the configured assets dir,
    // overridable via EQ_SPELLS_FILE.
    let spells_path = std::env::var("EQ_SPELLS_FILE")
        .unwrap_or_else(|_| data_dir.join("spells_us.txt").to_string_lossy().into_owned());
    let spells: std::sync::Arc<eqoxide::spells::SpellDb> =
        std::sync::Arc::new(eqoxide::spells::SpellDb::load(&spells_path));
    let shared_collision: assets::SharedCollision = Arc::new(std::sync::RwLock::new(None));
    let frame_req:        http::FrameReq        = Arc::new(Mutex::new(None));
    let player_info:      http::PlayerInfo      = Arc::new(Mutex::new(http::PlayerState::default()));

    // EQ network task — skipped in --testzone mode (offline debug)
    let character_name = login_cfg.character_name.clone();
    let asset_user     = login_cfg.username.clone();
    let asset_pass     = login_cfg.password.clone();
    let asset_server_url = app_cfg.asset_server_url.clone();
    if !testzone_mode {
        let gt  = goto_target.clone();
        let ep  = entity_positions.clone();
        let ei  = entity_ids.clone();
        let zp  = zone_points.clone();
        let tl  = task_log.clone();
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
        let ca  = cast.clone();
        let ms  = mem_spell.clone();
        let st  = sit.clone();
        let co  = consider.clone();
        let sc  = shared_collision.clone();
        let sd  = shutdown.clone();
        let md  = data_dir.join("maps");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                if let Err(e) = eq_net::run_login_flow(login_cfg, app_tx, 10, gt, ep, ei, zp, tl, zc, hl, sy, tg, at, by, sl, tr, mc, mv, gv, iv, lt, dc, ds, mg, ca, ms, st, co, sc, md, sd).await {
                    tracing::error!("EQ: fatal: {e}");
                }
            });
        });
    }

    // HTTP server
    let app_goto = goto_target.clone();
    let app_hail   = hail.clone();
    let app_say    = say.clone();
    let app_target = target.clone();
    let app_attack  = attack.clone();
    let app_cast    = cast.clone();
    let app_sit     = sit.clone();
    let app_consider = consider.clone();
    let app_buy     = buy.clone();
    let app_sell    = sell.clone();
    let app_trade   = trade.clone();
    let app_spells  = spells.clone();
    let app_door_click = door_click.clone();
    let app_player_info = player_info.clone();
    http::spawn_camera_server(
        camera_cmd.clone(),
        camera_snapshot.clone(),
        frame_req.clone(),
        goto_target,
        entity_positions,
        entity_ids,
        zone_points,
        zone_cross,
        warp.clone(),
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
        spells.clone(),
        player_info,
        task_log,
        door_click,
        doors_shared,
        shutdown.clone(),
        app_cfg.http_port,
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
        app_hail,
        app_say,
        app_target,
        app_attack,
        app_cast,
        app_sit,
        app_consider,
        app_buy,
        app_sell,
        app_trade,
        app_spells,
        app_door_click,
        shared_collision,
        app_player_info,
        warp,
        testzone_mode,
        shutdown.clone(),
        asset_server_url,
        asset_user,
        asset_pass,
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
