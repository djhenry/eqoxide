//! #643 — `Entity.animation` was field-overloaded: a POSE enum when written at spawn, a GAIT
//! percentage when written by a position update, with the meaning decided by whichever packet
//! arrived last. The renderer's `_ => "idle"` catch-all then turned everything it could not
//! classify into a plausible-looking default, so the client reported a moving NPC as idle and
//! could never render an NPC that sat down after it had moved.
//!
//! These tests drive the **real production packet-apply path** (`eq_net::packet_handler::apply_packet`
//! → `GameState`) and then assert on the two real observables: the renderer's clip selection
//! (`SceneState::from_game_state`) and the actual serialized `/v1/observe/entities?labeled=1`
//! HTTP body. They live in the app crate because `eq_net` (packet apply), `eqoxide-renderer`
//! (scene projection) and `eqoxide-http` (the router) are three sibling crates that are only in
//! scope together here.
//!
//! **Written to compile and run on `origin/main` too.** Every symbol used in the packet/scene
//! tests below (`register_spawn`, `SpawnInfo`, `apply_packet`, `encode_position_update`,
//! `build_spawn_appearance_packet`, `SceneState::from_game_state`, `Billboard.action`) exists
//! unchanged on `d5fe106` — deliberately, so the RED-on-main claim is checkable by copying this
//! file onto main rather than by reasoning about it.

use eqoxide::eq_net::packet_handler::{apply_packet, register_spawn};
use eqoxide::eq_net::protocol::{
    build_spawn_appearance_packet, encode_position_update, SpawnInfo, OP_CLIENT_UPDATE,
    OP_SPAWN_APPEARANCE,
};
use eqoxide::scene::SceneState;

/// A minimal living NPC spawn with a caller-chosen pose byte (`stand_state`).
/// `stand_state` 100 = `Animation::Standing`, 110 = `Animation::Sitting`.
fn npc_spawn(spawn_id: u32, name: &str, stand_state: u8) -> SpawnInfo {
    SpawnInfo {
        spawn_id,
        name: name.into(),
        last_name: String::new(),
        level: 5,
        npc: 1,
        gender: 0,
        race: 54,
        class_: 1,
        guild_id: 0xFFFF_FFFF,
        guild_rank: 0,
        body_type: 1,
        cur_hp: 100,
        helm: 0,
        show_helm: false,
        face: 0,
        hairstyle: 0,
        haircolor: 0,
        stand_state,
        flymode: 0,
        pet_owner_id: 0,
        player_state: 64,
        x: 10.0,
        y: 20.0,
        z: 5.0,
        heading: 0.0,
        // The spawn struct's OWN 10-bit gait field. EQEmu sends 0 here (`ns->spawn.animation = 0`),
        // which is exactly why the two domains were so easy to confuse: at spawn the gait slot is
        // empty and the pose lives in `stand_state`.
        animation: 0,
        equipment: [0u32; 9],
        equipment_tint: [[0u8; 3]; 9],
    }
}

/// A real 24-byte RoF2 `OP_ClientUpdate` for `spawn_id`, carrying gait code `gait` in the 10-bit
/// `animation` sub-field of word4 (bytes 20..24) — the field this client's own outbound encoder
/// (`action_loop::speed_to_wire_animation`) fills with 12 at walkspeed and 28 at full run.
fn position_update(spawn_id: u16, x: f32, y: f32, z: f32, gait: u32) -> Vec<u8> {
    let mut p = encode_position_update(spawn_id, x, y, z, 0.0);
    p[20..24].copy_from_slice(&gait.to_le_bytes()); // word4: animation:10 at bits 0-9
    p
}

/// Run the packet through the REAL opcode dispatcher (`apply_packet`), not a private handler —
/// so the test also proves the opcode is actually routed, not just that a handler exists.
fn feed(gs: &mut eqoxide::game_state::GameState, opcode: u16, payload: &[u8]) {
    apply_packet(gs, &eqoxide::eq_net::transport::AppPacket { opcode, payload: payload.to_vec() });
}

/// The action string the renderer would pick for `spawn_id` this frame.
fn rendered_action(gs: &eqoxide::game_state::GameState, spawn_id: u32) -> String {
    let scene = SceneState::from_game_state(gs, &std::collections::HashMap::new());
    scene
        .billboards
        .iter()
        .find(|b| b.id == spawn_id)
        .unwrap_or_else(|| panic!("spawn {spawn_id} missing from the scene"))
        .action
        .clone()
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Call site A — the domain split. A position update must write GAIT and must NOT destroy POSE.
// Isolated here from call site B (the new AT_Anim writer) so each can be mutation-checked alone.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// An NPC that the server told us was SITTING at spawn, and which then sends a position update,
/// must still render as sitting.
///
/// RED on main: `apply_position_update` wrote `upd.animation` (gait, here 12) into the same
/// `e.animation` field that held the pose (110), so `scene.rs`'s pose match fell straight through
/// to `_ => "idle"`. This test needs ONLY the domain split — no AT_Anim packet is sent.
#[test]
fn position_update_does_not_destroy_the_spawn_pose_643() {
    let mut gs = eqoxide::game_state::GameState::new();
    gs.player_name = "Somebody_Else".into();
    gs.player_id = 1;
    register_spawn(&mut gs, npc_spawn(42, "a_gnoll_pup", 110)); // Animation::Sitting

    assert_eq!(rendered_action(&gs, 42), "sitting", "test premise: it spawned sitting");

    // One position update — the entity shuffles a little, gait 12 (native walkspeed).
    feed(&mut gs, OP_CLIENT_UPDATE, &position_update(42, 11.0, 21.0, 5.0, 12));

    assert_eq!(
        rendered_action(&gs, 42),
        "sitting",
        "a position update carries GAIT, not a pose — it must not overwrite the sitting pose (#643)"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Call site B — the missing pose writer. OP_SpawnAppearance type 14 about ANOTHER spawn is the
// server's ONLY pose-change broadcast (EQEmu `Mob::SetAppearance` → `SendAppearancePacket`).
// Before #643 this branch did not exist, so no NPC could ever change pose after spawning.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// An NPC that spawns standing and is then broadcast as sitting must render as sitting.
/// No position update is involved, so this pins the AT_Anim writer alone.
///
/// RED on main: `apply_spawn_appearance` only acted on `id == gs.player_id`; another spawn's
/// pose change was dropped on the floor entirely and the NPC kept rendering `"idle"`.
#[test]
fn spawn_appearance_anim_sits_another_entity_643() {
    let mut gs = eqoxide::game_state::GameState::new();
    gs.player_name = "Somebody_Else".into();
    gs.player_id = 1;
    register_spawn(&mut gs, npc_spawn(42, "a_gnoll_pup", 100)); // Animation::Standing
    assert_eq!(rendered_action(&gs, 42), "idle", "test premise: standing renders as the idle clip");

    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(42, 14, 110));

    assert_eq!(
        rendered_action(&gs, 42),
        "sitting",
        "OP_SpawnAppearance type 14 param 110 about another spawn is the server telling us that \
         NPC sat down — it must reach the renderer (#643)"
    );
}

/// The other pose codes on the same channel resolve to their own clips, and an unrecognised code
/// does NOT silently become standing/idle-with-no-trace: it is retained as `unknown(<raw>)` and
/// surfaced through the API (asserted in the HTTP test below).
#[test]
fn spawn_appearance_anim_covers_the_other_pose_codes_643() {
    let mut gs = eqoxide::game_state::GameState::new();
    gs.player_name = "Somebody_Else".into();
    gs.player_id = 1;
    register_spawn(&mut gs, npc_spawn(42, "a_gnoll_pup", 100));

    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(42, 14, 111)); // Crouching
    assert_eq!(rendered_action(&gs, 42), "crouching");

    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(42, 14, 100)); // Standing
    assert_eq!(rendered_action(&gs, 42), "idle");
}

/// A dead entity must stay lying down: a stray appearance packet must not stand a corpse up.
///
/// The RENDER assertion alone is NOT mutation-discriminating — `scene.rs` short-circuits on
/// `e.dead` before it ever looks at the pose, so deleting the `!e.dead` guard leaves the drawn clip
/// unchanged (verified: mutation M3 left this test green when it asserted only the clip). The guard
/// is there for the AGENT-FACING pose field, so that is what this test pins.
#[test]
fn spawn_appearance_anim_cannot_stand_a_corpse_up_643() {
    let mut gs = eqoxide::game_state::GameState::new();
    gs.player_name = "Somebody_Else".into();
    gs.player_id = 1;
    // `npc: 3` is the corpse body flag `register_spawn` lays down at zone-in.
    let mut corpse = npc_spawn(42, "a_gnoll_pup's corpse", 100);
    corpse.npc = 3;
    register_spawn(&mut gs, corpse);
    assert_eq!(rendered_action(&gs, 42), "dead", "test premise: it spawned as a corpse");

    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(42, 14, 100));

    assert_eq!(rendered_action(&gs, 42), "dead", "a corpse must not be stood back up (#643)");
    assert_eq!(
        gs.world.entities[&42].pose.label(),
        "lying",
        "the corpse's REPORTED pose must stay lying too — this is the assertion the `!e.dead` \
         guard actually protects; the rendered clip is short-circuited by `e.dead` regardless"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// Both call sites together — the canonical live case from the issue.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// **The canonical symptom**: an NPC that you have already watched MOVE, and which then sits down,
/// must render as sitting. This needs BOTH halves of the fix — the AT_Anim writer to deliver the
/// pose change at all, AND the domain split so the position update did not already destroy the
/// field's meaning. RED on main for both reasons.
#[test]
fn npc_that_moves_then_sits_renders_sitting_643() {
    let mut gs = eqoxide::game_state::GameState::new();
    gs.player_name = "Somebody_Else".into();
    gs.player_id = 1;
    register_spawn(&mut gs, npc_spawn(42, "a_gnoll_pup", 100));

    // It walks a few steps (gait 12 = native walkspeed), the way any live NPC does.
    for i in 1..=3 {
        feed(&mut gs, OP_CLIENT_UPDATE, &position_update(42, 10.0 + i as f32, 20.0, 5.0, 12));
    }
    // Then it sits down.
    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(42, 14, 110));

    assert_eq!(
        rendered_action(&gs, 42),
        "sitting",
        "an NPC that sits down AFTER you have seen it move must render sitting — this is the \
         exact case #643 says the client could never render"
    );
}

// ─────────────────────────────────────────────────────────────────────────────────────────────
// The agent-facing half — assert on the REAL serialized HTTP body from the REAL router.
// ─────────────────────────────────────────────────────────────────────────────────────────────

/// Before #643 the HTTP API exposed NO pose or gait field for any entity at all, so an agent
/// driving this client had no channel whatsoever to the state the renderer was quietly guessing
/// at. This asserts the real `/v1/observe/entities?labeled=1` JSON body — not an internal struct —
/// carries both domains, separately and correctly, after the same move-then-sit sequence.
///
/// NOTE ON MUTATION SCOPE: this test's setup seeds `world.entity_poses` the way the net thread's
/// `ActionLoop::sync_entities` does; that the REAL `sync_entities` publishes it is pinned
/// separately by `sync_entities_publishes_pose_and_gait_643` in `eqoxide-net/src/action_loop.rs`.
/// Neither test alone covers the whole net→HTTP path; together they cover it end to end, and this
/// boundary is stated rather than glossed over.
#[tokio::test]
async fn entities_labeled_body_reports_pose_and_gait_separately_643() {
    use eqoxide::http::testkit::{empty_state, observe_json, world_slots};

    let mut gs = eqoxide::game_state::GameState::new();
    gs.player_name = "Somebody_Else".into();
    gs.player_id = 1;
    register_spawn(&mut gs, npc_spawn(42, "a_gnoll_pup", 100));
    feed(&mut gs, OP_CLIENT_UPDATE, &position_update(42, 11.0, 20.0, 5.0, 12));
    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(42, 14, 110));
    // A second NPC the server described with a pose code we do not know — it must be reported as
    // unknown, NOT quietly folded into "standing".
    register_spawn(&mut gs, npc_spawn(43, "a_gnoll_elder", 100));
    feed(&mut gs, OP_SPAWN_APPEARANCE, &build_spawn_appearance_packet(43, 14, 199));

    let state = empty_state();
    {
        let world = world_slots(&state);
        let mut pos = world.entity_positions.lock().unwrap();
        let mut poses = world.entity_poses.lock().unwrap();
        for e in gs.world.entities.values() {
            pos.insert(e.name.clone(), (e.x, e.y, e.z));
            poses.insert(
                e.name.clone(),
                eqoxide::ipc::EntityPoseView { pose: e.pose.label(), gait: e.gait.map(|g| g.raw()) },
            );
        }
    }

    let body = observe_json(state, "/entities?labeled=1").await;
    let poses = &body["poses"];

    assert_eq!(
        poses["a_gnoll_pup"]["pose"],
        serde_json::json!("sitting"),
        "the agent must be able to READ that the NPC is sitting (#643)"
    );
    assert_eq!(
        poses["a_gnoll_pup"]["gait"],
        serde_json::json!(12),
        "gait is its own field and keeps the wire's own locomotion signal (#643)"
    );
    assert_eq!(
        poses["a_gnoll_elder"]["pose"],
        serde_json::json!("unknown(199)"),
        "an unrecognised pose code must be reported verbatim, never guessed at (agent-honesty)"
    );
    assert_eq!(
        poses["a_gnoll_elder"]["gait"],
        serde_json::json!(null),
        "no position update yet => gait null ('not reported'), NOT 0 ('standing still')"
    );
}
