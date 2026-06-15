mod app;
mod assets;
mod anim;
mod billboard;
mod camera;
mod camera_state;
mod config;
mod debug_zone;
mod eq_net;
mod frame_capture;
mod game_state;
mod gpu;
mod http;
mod hud;
mod models;
mod pass;
mod pipeline;
mod renderer;
mod scene;
mod zone_map;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use winit::event_loop::EventLoop;

fn main() {
    let login_cfg = config::LoginConfig::load();
    let app_cfg   = config::AppConfig::load();

    let camera_cmd: Arc<Mutex<Option<camera_state::CameraCmd>>> = Arc::new(Mutex::new(None));
    let camera_snapshot: Arc<Mutex<camera_state::CameraSnapshot>> = Arc::new(Mutex::new(
        camera_state::CameraState::new([0.0, 0.0, 0.0], 0.0).snapshot(),
    ));

    let (app_tx, app_rx) = tokio::sync::mpsc::unbounded_channel::<eq_net::AppPacket>();
    let goto_target:      http::GotoTarget      = Arc::new(Mutex::new(None));
    let entity_positions: http::EntityPositions = Arc::new(Mutex::new(HashMap::new()));
    let zone_points:      http::ZonePoints      = Arc::new(Mutex::new(Vec::new()));
    let zone_cross:       http::ZoneCrossReq    = Arc::new(Mutex::new(false));
    let hail:             http::HailReq         = Arc::new(Mutex::new(None));
    let say:              http::SayReq          = Arc::new(Mutex::new(None));
    let target:           http::TargetReq       = Arc::new(Mutex::new(None));
    let frame_req:        http::FrameReq        = Arc::new(Mutex::new(None));

    // EQ network task
    let character_name = login_cfg.character_name.clone();
    let gt  = goto_target.clone();
    let ep  = entity_positions.clone();
    let zp  = zone_points.clone();
    let zc  = zone_cross.clone();
    let hl  = hail.clone();
    let sy  = say.clone();
    let tg  = target.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().expect("tokio runtime");
        rt.block_on(async {
            if let Err(e) = eq_net::run_login_flow(login_cfg, app_tx, 10, gt, ep, zp, zc, hl, sy, tg).await {
                eprintln!("EQ: fatal: {e}");
            }
        });
    });

    // HTTP server
    let app_goto = goto_target.clone();
    let app_hail   = hail.clone();
    let app_say    = say.clone();
    let app_target = target.clone();
    http::spawn_camera_server(
        camera_cmd.clone(),
        camera_snapshot.clone(),
        frame_req.clone(),
        goto_target,
        entity_positions,
        zone_points,
        zone_cross,
        hail,
        say,
        target,
        app_cfg.http_port,
    );

    let event_loop = EventLoop::new().expect("event loop");
    let mut application = app::App::new(
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
    );
    event_loop.run_app(&mut application).expect("event loop run");
}
