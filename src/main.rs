//! Full-client entry point.
//!
//! Loads config + the EQ string table, creates the shared request slots (`Arc<Mutex<…>>`) and the
//! mpsc packet channel, then starts the three concurrent halves: the EQ network thread
//! (`run_login_flow`, skipped with `--testzone`), the HTTP API server, and the winit/wgpu render
//! loop on the main thread. The request slots are the cross-thread glue — HTTP writes them, the nav
//! thread drains them. `--testzone` runs the renderer offline (no server) for asset/zone debugging.

use eqoxide::{camera_state, config, eq_net, eqstr, http, ipc};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use winit::event_loop::EventLoop;

// Shared by `parse_cli` (prints it on a parse error / `-h`/`--help`) and `main` (prints it when a
// syntactically valid `--config` still doesn't resolve to a file). Keeping one copy means both
// error paths can never drift apart.
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

/// Parsed command-line flags, as produced by [`parse_cli`].
struct CliArgs {
    testzone: bool,
    profile: bool,
    nav_debug: bool,
    config: Option<String>,
    api_port: Option<u16>,
}

/// Parse + STRICTLY validate `std::env::args()`. Errors out (printing [`USAGE`] and exiting via
/// `eqoxide::crash::exit`) on anything malformed or unrecognized rather than silently falling
/// back to defaults — a silent fallback once made the client log into the wrong account when
/// `--config` pointed at a missing file.
fn parse_cli() -> CliArgs {
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
            "-h" | "--help" => { print!("{USAGE}"); eqoxide::crash::exit("help", 0); }
            // accept both "--config <value>" and "--config=<value>"
            _ if arg == "--config" || arg.starts_with("--config=") => {
                let value = if let Some(v) = arg.strip_prefix("--config=") {
                    v.to_string()
                } else {
                    match args.get(idx + 1) {
                        Some(v) if !v.starts_with('-') => { idx += 1; v.clone() }
                        _ => {
                            eprintln!("error: --config requires a value (a profile name or config file path)\n\n{USAGE}");
                            eqoxide::crash::exit("bad-args", 2);
                        }
                    }
                };
                if value.is_empty() {
                    eprintln!("error: --config requires a non-empty value\n\n{USAGE}");
                    eqoxide::crash::exit("bad-args", 2);
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
                            eqoxide::crash::exit("bad-args", 2);
                        }
                    }
                };
                match value.parse::<u16>() {
                    Ok(p) if p > 0 => api_port_arg = Some(p),
                    _ => {
                        eprintln!("error: --api-port must be a number 1-65535, got '{value}'\n\n{USAGE}");
                        eqoxide::crash::exit("bad-args", 2);
                    }
                }
            }
            other => {
                eprintln!("error: unrecognized argument '{other}'\n\n{USAGE}");
                eqoxide::crash::exit("bad-args", 2);
            }
        }
        idx += 1;
    }
    CliArgs {
        testzone: testzone_mode,
        profile: profile_flag,
        nav_debug: nav_debug_flag,
        config: login_cfg_arg,
        api_port: api_port_arg,
    }
}

fn main() {
    eqoxide::logging::init();
    // Install the panic hook + fatal-signal handlers + heartbeat BEFORE anything else runs, so
    // no thread can panic or fault before the client is able to say why (#380).
    eqoxide::crash::install();

    let cli = parse_cli();
    let testzone_mode = cli.testzone;
    let nav_debug_flag = cli.nav_debug;

    // `--profile` (or EQ_PROFILE=1) enables the lightweight per-phase frame-timing HUD overlay.
    let profile_mode = cli.profile
        || std::env::var("EQ_PROFILE").map(|v| v != "0" && !v.is_empty()).unwrap_or(false);
    eqoxide::profiling::set_enabled(profile_mode);

    // Resolve the login config. When --config is given the resolved file MUST exist — we never fall
    // back to the default config in that case. The default ~/.config/eqoxide/config.yaml is used
    // only when --config is omitted.
    let login_cfg_path = config::LoginConfig::resolve_path(cli.config.as_deref());
    if cli.config.is_some() && !login_cfg_path.exists() {
        eprintln!("error: config file not found for --config {}: {}\n\n{USAGE}",
            cli.config.as_deref().unwrap_or(""), login_cfg_path.display());
        eqoxide::crash::exit("bad-args", 2);
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

    // Shared clean-shutdown flag. Set by window-close, a completed camp, and signals; observed by
    // the EQ network thread, which performs the logout sequence and exits the process.
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

    // ── M4: domain slot bundles (see `ipc.rs`) ──────────────────────────────────────────────
    // Each bundle below is constructed EXACTLY ONCE here and `.clone()`d (a shallow Arc-handle
    // clone — same underlying channel) into every consumer that needs it: the `eq-net` thread's
    // `ActionLoop` (via `run_login_flow`), `HttpState` (via `spawn_camera_server`), the pre-existing
    // `ui::Actions` bundle, and/or `App::new`. This is what preserves the cross-thread sharing that
    // used to be implicit in each individual `Arc::new(...)` being cloned by hand — see the
    // per-bundle `.clone()` calls below; nothing here is `Default`-constructed twice.
    let camera = ipc::CameraSlots {
        cmd_tx:      Arc::new(Mutex::new(None)),
        snapshot:    Arc::new(Mutex::new(camera_state::CameraState::new([0.0, 0.0, 0.0], 0.0).snapshot())),
        frame_req:   Arc::new(Mutex::new(None)),
        manual_move: Arc::new(Mutex::new(None)),
    };
    let nav = ipc::NavSlots {
        goto_target:   Arc::new(Mutex::new(None)),
        goto_entity:   Arc::new(Mutex::new(None)),
        zone_cross:    Arc::new(Mutex::new(None)),
        nav_avoid:     Arc::new(Mutex::new(ipc::AggroAvoidOpts::default())),
        nav_state:     Arc::new(Mutex::new(ipc::NavStatus::default())),
        nav_path_view: Arc::new(Mutex::new((Vec::new(), Vec::new()))),
    };
    let world = ipc::WorldSlots {
        entity_positions: Arc::new(Mutex::new(HashMap::new())),
        entity_ids:       Arc::new(Mutex::new(HashMap::new())),
        zone_points:      Arc::new(Mutex::new(Vec::new())),
    };
    let quest = ipc::QuestSlots {
        task_log:               Arc::new(Mutex::new(Vec::new())),
        task_offers_shared:     Arc::new(Mutex::new(Vec::new())),
        completed_tasks_shared: Arc::new(Mutex::new(Vec::new())),
        accept_task:            Arc::new(Mutex::new(None)),
        cancel_task:            Arc::new(Mutex::new(None)),
    };
    let group_slots = ipc::GroupSlots {
        group:             Arc::new(Mutex::new(ipc::GroupSnapshot::default())),
        group_invite:      Arc::new(Mutex::new(None)),
        group_accept:      Arc::new(Mutex::new(None)),
        group_decline:     Arc::new(Mutex::new(None)),
        group_leave:       Arc::new(Mutex::new(None)),
        group_kick:        Arc::new(Mutex::new(None)),
        group_make_leader: Arc::new(Mutex::new(None)),
    };
    let trainer = ipc::TrainerSlots {
        trainer_open_req:  Arc::new(Mutex::new(None)),
        trainer_train_req: Arc::new(Mutex::new(None)),
    };
    let combat = ipc::CombatSlots {
        attack:    Arc::new(Mutex::new(None)),
        cast:      Arc::new(Mutex::new(None)),
        mem_spell: Arc::new(Mutex::new(None)),
        consider:  Arc::new(Mutex::new(None)),
        target:    Arc::new(Mutex::new(None)),
        pet_cmd:   Arc::new(Mutex::new(None)),
    };
    let social = ipc::SocialSlots {
        who_req:      Arc::new(Mutex::new(None)),
        // Client-local friends list + its presence-poll request slot (#301).
        friends_list: Arc::new(Mutex::new(Vec::new())),
        friends_req:  Arc::new(Mutex::new(None)),
    };
    let merchant_slots = ipc::MerchantSlots {
        merchant: Arc::new(Mutex::new(ipc::MerchantSnapshot::default())),
        buy:      Arc::new(Mutex::new(None)),
        sell:     Arc::new(Mutex::new(None)),
        trade:    Arc::new(Mutex::new(None)),
    };
    let inventory_slots = ipc::InventorySlots {
        inventory: Arc::new(Mutex::new(Vec::new())),
        move_req:  Arc::new(Mutex::new(None)),
    };
    let interact = ipc::InteractSlots {
        hail:           Arc::new(Mutex::new(None)),
        say:            Arc::new(Mutex::new(None)),
        loot:           Arc::new(Mutex::new(None)),
        give:           Arc::new(Mutex::new(None)),
        door_click:     Arc::new(Mutex::new(None)),
        doors_shared:   Arc::new(Mutex::new(Vec::new())),
        sit:            Arc::new(Mutex::new(None)),
        dialogue:       Arc::new(Mutex::new(Vec::new())),
        dialogue_click: Arc::new(Mutex::new(None)),
        // POST /v1/interact/read request slot (#288): the inventory wire slot of a book/note to read.
        read_book:      Arc::new(Mutex::new(None)),
    };
    let chat = ipc::ChatSlots {
        chat_events: Arc::new(Mutex::new(Vec::new())),
        chat_send:   Arc::new(Mutex::new(Vec::new())),
        messages:    Arc::new(Mutex::new(Vec::new())),
    };
    // Single-authority movement (Component A): the render thread owns the CharacterController and
    // publishes `controller_view`; the nav thread streams it and writes `nav_intent` for /goto;
    // `pos_correction` hands a server correction back to the controller. Consumed by `ActionLoop`
    // and `App` — NOT by `HttpState` (no /v1/* route reads it directly).
    let controller = ipc::ControllerSlots {
        controller_view: Arc::new(Mutex::new(eqoxide::movement::ControllerView::default())),
        nav_intent:      Arc::new(Mutex::new(None)),
        pos_correction:  Arc::new(Mutex::new(None)),
    };
    // Guild roster/identity published for GET /v1/guild/roster + /observe/debug, and the guild-action
    // request slot for POST /v1/guild/{invite,accept,leave,remove} (#295).
    let guild_slots = ipc::GuildSlots {
        guild:        Arc::new(Mutex::new(ipc::GuildSnapshot::default())),
        guild_action: Arc::new(Mutex::new(None)),
    };
    // Camp slots. `camp` carries a pending camp command (/exit, /camp, HUD button, `/camp` chat);
    // `camp_until` is the published camp deadline (Some while camping) for the HUD countdown.
    // Respawn (#284): POST /v1/lifecycle/respawn sets `respawn`; the gameplay loop reads it to
    // release a held-dead character to its bind point (no more auto-respawn).
    let lifecycle = ipc::LifecycleSlots {
        camp:       Arc::new(Mutex::new(None)),
        camp_until: Arc::new(Mutex::new(None)),
        respawn:    Arc::new(Mutex::new(false)),
    };

    // #446: the typed write-path facade over the command/action slots. Constructed ONCE here
    // (Controller/wiring role) from `.clone()`s of the SAME command bundles above, then handed to
    // both `ActionLoop` (via `run_login_flow`) and `HttpState` (via `spawn_camera_server`) and the
    // UI `Actions` bundle — so a `request_*` write and its `take_*` drain share the same slot. Combat
    // is fully migrated onto it; the other domains keep their bundle fields until Wave-2 migrates
    // them (see `crate::command_state`).
    let command = eqoxide::command_state::CommandState::new(
        combat.clone(), merchant_slots.clone(), inventory_slots.clone(), interact.clone(),
        quest.clone(), group_slots.clone(), guild_slots.clone(), trainer.clone(), social.clone(),
        chat.clone(), nav.clone(), lifecycle.clone(), camera.manual_move.clone(),
    );

    // spells_us.txt is an EQ data file; default to the configured assets dir,
    // overridable via EQ_SPELLS_FILE.
    let spells_path = std::env::var("EQ_SPELLS_FILE")
        .unwrap_or_else(|_| data_dir.join("spells_us.txt").to_string_lossy().into_owned());
    let spells: std::sync::Arc<eqoxide::spells::SpellDb> =
        std::sync::Arc::new(eqoxide::spells::SpellDb::load(&spells_path));
    // Publish globally so the nav thread can resolve spell target types for self-cast (eqoxide#95).
    eqoxide::spells::set_global(spells.clone());
    let shared_collision: eqoxide::nav::collision::SharedCollision = Arc::new(std::sync::RwLock::new(None));
    // Single-owner GameState snapshot (see
    // docs/superpowers/plans/2026-07-12-gamestate-single-owner-snapshot.md). The network thread is
    // the sole writer of GameState; it publishes here every tick. `last_inbound` is a separate,
    // smaller signal: the wall-clock time of the last REAL inbound packet, used for connection
    // health (a hung network thread stops updating it even though nothing else changes).
    let game_state_snapshot: ipc::GameStateSnapshot =
        Arc::new(arc_swap::ArcSwap::from_pointee(eqoxide::game_state::GameState::new()));
    // The network thread's three liveness clocks (link / application packet / gameplay tick). The
    // HTTP layer turns them into `connected`, `last_packet_age_ms` and `snapshot_age_ms` at READ
    // time, so a frozen world can never masquerade as a live one (#343).
    let net_health_shared: ipc::NetHealthShared =
        Arc::new(Mutex::new(ipc::NetHealth::default()));
    // Render-owned frame timings — the one agent-visible value the render loop publishes (#343).
    let frame_profile_shared: ipc::FrameProfileShared =
        Arc::new(Mutex::new(eqoxide::profiling::FrameProfile::default()));

    // EQ network task — skipped in --testzone mode (offline debug)
    let character_name = login_cfg.character_name.clone();
    let asset_user     = login_cfg.username.clone();
    let asset_pass     = login_cfg.password.clone();
    let asset_server_url = app_cfg.asset_server_url.clone();
    if !testzone_mode {
        // Each bundle below is a shallow `.clone()` of the Arc handles constructed once above —
        // this thread gets the SAME underlying channels as `HttpState` (wired below), preserving
        // the cross-thread sharing the flat per-field clones used to provide.
        let nav_b             = nav.clone();
        let world_b           = world.clone();
        let quest_b           = quest.clone();
        let group_slots_b     = group_slots.clone();
        let command_b         = command.clone();
        let social_b          = social.clone();
        let merchant_slots_b  = merchant_slots.clone();
        let inventory_slots_b = inventory_slots.clone();
        let interact_b        = interact.clone();
        let chat_b            = chat.clone();
        let controller_b      = controller.clone();
        let guild_slots_b     = guild_slots.clone();
        let sc  = shared_collision.clone();
        let sd  = shutdown.clone();
        let cp  = lifecycle.camp.clone();
        let cu  = lifecycle.camp_until.clone();
        let rsp = lifecycle.respawn.clone();
        let gss = game_state_snapshot.clone();
        let nh  = net_health_shared.clone();
        let md  = data_dir.join("maps");
        // Named (not the default anonymous thread) so a panic here — the exact "worker thread
        // dies quietly" risk #380 calls out — identifies itself in the crash log instead of
        // showing up as thread '<unnamed>'. Its own tokio runtime's worker pool is named
        // distinctly from the HTTP server's (see below) for the same reason.
        std::thread::Builder::new()
            .name("eq-net".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .thread_name("eq-net-tokio-worker")
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async {
                    if let Err(e) = eq_net::run_login_flow(
                        login_cfg, 10,
                        nav_b, world_b, quest_b, group_slots_b, command_b, social_b,
                        merchant_slots_b, inventory_slots_b, interact_b, chat_b, controller_b,
                        guild_slots_b, sc, md, sd, cp, cu, rsp, gss, nh,
                    ).await {
                        tracing::error!("EQ: fatal: {e}");
                    }
                });
            })
            .expect("spawn eq-net thread");
    }

    // HTTP server
    let app_goto = nav.goto_target.clone();
    // All the request slots UI windows can write, bundled (#162). These are the
    // same slots the HTTP API and nav/gameplay threads share. This is a PRE-EXISTING bundle
    // (separate from the M4 domain bundles above); its fields are individual `.clone()`s pulled
    // out of the M4 bundles so it keeps sharing the same underlying Arcs.
    let app_actions = eqoxide::ui::Actions {
        command: command.clone(),
        hail: interact.hail.clone(),
        say: interact.say.clone(),
        chat_send: chat.chat_send.clone(),
        dialogue_click: interact.dialogue_click.clone(),
        sit: interact.sit.clone(),
        move_item: inventory_slots.move_req.clone(),
        loot: interact.loot.clone(),
        accept_task: quest.accept_task.clone(),
        cancel_task: quest.cancel_task.clone(),
        group_invite: group_slots.group_invite.clone(),
        group_accept: group_slots.group_accept.clone(),
        group_decline: group_slots.group_decline.clone(),
        group_leave: group_slots.group_leave.clone(),
        group_kick: group_slots.group_kick.clone(),
        group_make_leader: group_slots.group_make_leader.clone(),
        camp_until: lifecycle.camp_until.clone(),
    };
    let app_spells  = spells.clone();
    let app_frame_profile = frame_profile_shared.clone();
    // --api-port N: bind exactly N now and FAIL THE LAUNCH if it's taken (don't open a window with
    // a dead API). The bound listener is handed to the server thread so there's no re-bind race.
    // Without --api-port, pass None and let the server scan upward from the config base port.
    let exact_listener: Option<std::net::TcpListener> = match cli.api_port {
        Some(p) => match std::net::TcpListener::bind(("127.0.0.1", p)) {
            Ok(l) => Some(l),
            Err(e) => {
                eprintln!("error: --api-port {p} is unavailable ({e}). Free the port or choose another.");
                eqoxide::crash::exit("api-port-unavailable", 1);
            }
        },
        None => None,
    };
    http::spawn_camera_server(
        camera.clone(),
        nav.clone(),
        world,
        shared_collision.clone(),
        command.clone(),
        social,
        merchant_slots,
        inventory_slots,
        interact.clone(),
        chat,
        spells.clone(),
        game_state_snapshot.clone(),
        net_health_shared.clone(),
        frame_profile_shared.clone(),
        quest,
        group_slots,
        lifecycle.clone(),
        guild_slots,
        app_cfg.http_port,
        exact_listener,
    );

    let event_loop = EventLoop::new().expect("event loop");
    let mut application = eqoxide::app::App::new(
        app_cfg.assets_path,
        app_cfg.models_path,
        character_name,
        camera.cmd_tx,
        camera.snapshot,
        game_state_snapshot.clone(),
        net_health_shared.clone(),
        camera.frame_req,
        app_goto,
        app_actions,
        app_spells,
        shared_collision,
        app_frame_profile,
        testzone_mode,
        nav_debug_flag,
        shutdown.clone(),
        app_cfg.eq_ui_dir,
        asset_server_url,
        asset_user,
        asset_pass,
        controller.controller_view,
        controller.nav_intent,
        controller.pos_correction,
        nav.nav_path_view,
    );
    event_loop.run_app(&mut application).expect("event loop run");
    // The event loop has now exited gracefully — either the window was closed, or a shutdown was
    // requested (POST /exit / OP_GMKick set the flag and `about_to_wait` called `event_loop.exit()`).
    // Either way winit has torn down its Wayland clipboard worker on this (main) thread, so it is now
    // safe to exit the process. Ensure the flag is set so the EQ network thread logs the character
    // out (it idles after sending OP_Logout + OP_SessionDisconnect), give it a moment, then exit.
    shutdown.store(true, Ordering::Relaxed);
    std::thread::sleep(std::time::Duration::from_millis(1500));
    // Record the clean exit BEFORE actually exiting (#380). Its presence as the last line of the
    // durable crash log is what makes its ABSENCE, after a run that's no longer running,
    // diagnostic of an unclean death (a panic/signal record would be there instead — or, for an
    // OOM-kill, neither, which the heartbeat file's last-write time can help distinguish).
    eqoxide::crash::exit("clean", 0);
}
