//! Full-client entry point.
//!
//! Loads config + the EQ string table, creates the shared request slots (`Arc<Mutex<…>>`) and the
//! mpsc packet channel, then starts the three concurrent halves: the EQ network thread
//! (`run_login_flow`, skipped with `--testzone`), the HTTP API server, and the winit/wgpu render
//! loop on the main thread. The request slots are the cross-thread glue — HTTP writes them, the nav
//! thread drains them. `--testzone` runs the renderer offline (no server) for asset/zone debugging.

use eq_renderer::{assets, camera_state, config, eq_net, eqstr, http};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use winit::event_loop::EventLoop;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let testzone_mode = args.contains(&"--testzone".to_string());

    let login_cfg = config::LoginConfig::load();
    let app_cfg   = config::AppConfig::load();

    // Load the EQ string table for OP_FormattedMessage / OP_SimpleMessage rendering.
    eqstr::load(&app_cfg.assets_path.join("eqstr_us.txt"));

    let camera_cmd: Arc<Mutex<Option<camera_state::CameraCmd>>> = Arc::new(Mutex::new(None));
    let camera_snapshot: Arc<Mutex<camera_state::CameraSnapshot>> = Arc::new(Mutex::new(
        camera_state::CameraState::new([0.0, 0.0, 0.0], 0.0).snapshot(),
    ));

    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<eq_net::AppPacket>();
    let goto_target:      http::GotoTarget      = Arc::new(Mutex::new(None));
    let entity_positions: http::EntityPositions = Arc::new(Mutex::new(HashMap::new()));
    let entity_ids:       http::EntityIds       = Arc::new(Mutex::new(HashMap::new()));
    let zone_points:      http::ZonePoints      = Arc::new(Mutex::new(Vec::new()));
    let zone_cross:       http::ZoneCrossReq    = Arc::new(Mutex::new(None));
    let warp:             http::WarpReq         = Arc::new(Mutex::new(None));
    let hail:             http::HailReq         = Arc::new(Mutex::new(None));
    let say:              http::SayReq          = Arc::new(Mutex::new(None));
    let target:           http::TargetReq       = Arc::new(Mutex::new(None));
    let attack:           http::AttackReq       = Arc::new(Mutex::new(None));
    let buy:              http::BuyReq          = Arc::new(Mutex::new(None));
    let shared_collision: assets::SharedCollision = Arc::new(std::sync::RwLock::new(None));
    let frame_req:        http::FrameReq        = Arc::new(Mutex::new(None));
    let player_info:      http::PlayerInfo      = Arc::new(Mutex::new(http::PlayerState::default()));

    // EQ network task — skipped in --testzone mode (offline debug)
    let character_name = login_cfg.character_name.clone();
    if !testzone_mode {
        let gt  = goto_target.clone();
        let ep  = entity_positions.clone();
        let ei  = entity_ids.clone();
        let zp  = zone_points.clone();
        let zc  = zone_cross.clone();
        let hl  = hail.clone();
        let sy  = say.clone();
        let tg  = target.clone();
        let at  = attack.clone();
        let by  = buy.clone();
        let sc  = shared_collision.clone();
        let md  = app_cfg.assets_path.join("maps");
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
            rt.block_on(async {
                if let Err(e) = eq_net::run_login_flow(login_cfg, app_tx, 10, gt, ep, ei, zp, zc, hl, sy, tg, at, by, sc, md).await {
                    eprintln!("EQ: fatal: {e}");
                }
            });
        });
    }

    // HTTP server
    let app_goto = goto_target.clone();
    let app_hail   = hail.clone();
    let app_say    = say.clone();
    let app_target = target.clone();
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
        buy,
        player_info,
        app_cfg.http_port,
    );

    let event_loop = EventLoop::new().expect("event loop");
    let mut application = eq_renderer::app::App::new(
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
        shared_collision,
        app_player_info,
        warp,
        testzone_mode,
    );
    event_loop.run_app(&mut application).expect("event loop run");
    // Exit cleanly so KDE doesn't report a crash when background threads
    // (EQ network, HTTP server) are still running at process teardown time.
    std::process::exit(0);
}
