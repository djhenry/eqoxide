//! Target / death STATE must reach `/observe/debug`, not just the `GameState` (#409/#406/#336).
//!
//! These exercise the REAL projection path the live bugs slip through: apply the actual packet
//! (OP_Consider / OP_Death) to a `GameState`, then hit the axum `/debug` route and assert the JSON.
//! The unit tests in `packet_handler.rs` mutate+read ONE `GameState` and so never see that the
//! hand-built `/debug` JSON dropped the field on the floor — that gap is exactly what #400 skipped
//! and what these close.
//!
//! They live in the APP crate (as an integration test) rather than inside `eqoxide-http` because
//! `apply_consider`/`apply_death` are app-layer packet-apply gameplay fns in `eq_net::packet_handler`,
//! which sits ABOVE `eqoxide-http` in the crate graph (#544 Step 2l). The `HttpState` builder + snapshot
//! seeding + `/debug` driver come from `eqoxide-http`'s `test-fixtures`-gated `testkit`; the packet-apply
//! fns come from the app crate. Both are only in scope together here. Preserved verbatim from the
//! `eqoxide-http` `observe::tests` module they were extracted from — none dropped.

use eqoxide::http::testkit::{debug_json, empty_state, set_gs};

/// OP_Consider reply (RoF2 Consider_Struct): playerid@0, targetid@4, faction@8, level@12.
fn consider_reply(player_id: u32, target_id: u32, faction: u32, level: u32) -> Vec<u8> {
    let mut p = vec![0u8; 20];
    p[0..4].copy_from_slice(&player_id.to_le_bytes());
    p[4..8].copy_from_slice(&target_id.to_le_bytes());
    p[8..12].copy_from_slice(&faction.to_le_bytes());
    p[12..16].copy_from_slice(&level.to_le_bytes());
    p
}

/// OP_Death (Death_S): spawn_id@0 (the dying entity), killer_id@4.
fn death_reply(spawn_id: u32, killer_id: u32) -> Vec<u8> {
    let mut p = vec![0u8; 32];
    p[0..4].copy_from_slice(&spawn_id.to_le_bytes());
    p[4..8].copy_from_slice(&killer_id.to_le_bytes());
    p
}

/// #409: after a successful consider of the CURRENT target, `/observe/debug` must expose the
/// structured con result — difficulty tier, attitude enum, and the target's level. On main these
/// are computed by the projection but NEVER serialized by `get_debug`, so an agent reads `null`
/// though `apply_consider` ran and the con succeeded. RED on main (fields absent), GREEN after.
#[tokio::test]
async fn debug_surfaces_consider_result_for_current_target_409() {
    let state = empty_state();
    set_gs(&state, |gs| {
        gs.player_id = 1;
        // The target's REAL level (12) comes from the spawn — deliberately different from the
        // consider reply's ConsiderColor field (13 = red) to prove the two are sourced separately.
        let mut npc = eqoxide::game_state::make_entity(136, "Caleah_Herblender000", 0.0, 0.0, 0.0, true);
        npc.level = 12;
        gs.upsert_entity(npc);
        gs.set_target(136);
        // faction 8 = "threatening", ConsiderColor 13 = "red".
        eqoxide::eq_net::packet_handler::apply_consider(gs, &consider_reply(gs.player_id, 136, 8, 13));
    });
    let p = debug_json(state).await["player"].clone();
    assert_eq!(p["target_con"], serde_json::json!("red"),
        "target_con must reach /observe/debug after a successful consider (#409)");
    assert_eq!(p["target_attitude"], serde_json::json!("threatening"),
        "target_attitude must reach /observe/debug after a successful consider (#409)");
    assert_eq!(p["target_level"], serde_json::json!(12),
        "target_level must reach /observe/debug (#409)");
}

/// #336: a STANDALONE consider (POST /v1/combat/consider {"id":N} on a spawn that is
/// deliberately NOT the current target) must be readable from `/observe/debug` too. The
/// target-scoped `player.target_con`/`target_attitude`/`target_level` stay null (correctly,
/// #330 — nothing is targeted), so the top-level `last_consider` object is the only surface
/// that can carry the result. RED before this fix (the tier was computed and discarded), GREEN
/// after.
#[tokio::test]
async fn debug_surfaces_last_consider_for_non_target_spawn_336() {
    let state = empty_state();
    set_gs(&state, |gs| {
        gs.player_id = 1;
        let mut npc = eqoxide::game_state::make_entity(450, "Guard_Phaeton", 0.0, 0.0, 0.0, true);
        npc.level = 20;
        gs.upsert_entity(npc);
        // no set_target — the whole point of the standalone endpoint.
        // faction 9 = "scowls", ConsiderColor 13 = "red".
        eqoxide::eq_net::packet_handler::apply_consider(gs, &consider_reply(gs.player_id, 450, 9, 13));
    });
    let v = debug_json(state).await;
    let p = v["player"].clone();
    assert_eq!(p["target_con"], serde_json::json!(null),
        "target_con must stay null — nothing is targeted (#330)");
    assert_eq!(p["target_attitude"], serde_json::json!(null));

    let lc = v["last_consider"].clone();
    assert_eq!(lc["spawn_id"], serde_json::json!(450),
        "last_consider must reach /observe/debug for a non-target spawn (#336)");
    assert_eq!(lc["con_name"], serde_json::json!("red"),
        "difficulty tier must be readable without targeting the spawn (#336)");
    assert_eq!(lc["attitude"], serde_json::json!("scowls"));
    assert_eq!(lc["level"], serde_json::json!(20));
    assert!(lc["ago_secs"].is_number(), "ago_secs must be present and numeric");
}

/// #406: after the character is slain (OP_Death for our own spawn), `/observe/debug` must report
/// `dead: true` + `killed_by` + `died_ago_secs`. On main the death message fires (log path) but
/// these STATE fields are never serialized by `get_debug`, so a held corpse reports `dead: null`
/// forever. RED on main (fields absent → null), GREEN after.
#[tokio::test]
async fn debug_surfaces_death_state_after_slain_406() {
    let state = empty_state();
    set_gs(&state, |gs| {
        gs.player_id = 42;
        gs.max_hp = 34;
        gs.upsert_entity(eqoxide::game_state::make_entity(66, "Guard_Doradek000", 0.0, 0.0, 0.0, true));
        eqoxide::eq_net::packet_handler::apply_death(gs, &death_reply(42, 66)); // our spawn, killed by 66
    });
    let p = debug_json(state).await["player"].clone();
    assert_eq!(p["dead"], serde_json::json!(true),
        "a slain character must report dead:true on /observe/debug (#406)");
    assert_eq!(p["killed_by"], serde_json::json!("Guard_Doradek000"),
        "killed_by must be surfaced so an agent knows to respawn (#406)");
    assert!(p["died_ago_secs"].as_u64().is_some(),
        "died_ago_secs must be present after a death (#406)");
}
