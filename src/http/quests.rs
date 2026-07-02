//! `/v1/quests/*` — native Task-system journal + old-style Lua turn-in quest givers.

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use std::collections::HashMap;
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/givers", get(get_givers))
        .route("/log", get(get_log))
        .route("/completed", get(get_completed))
        .route("/offers", get(get_offers))
        .route("/accept", post(post_accept))
        .route("/decline", post(post_decline))
        .route("/cancel", post(post_cancel))
}

/// GET /v1/quests/givers — the agent's "quests near me" view for the current zone. Lists quest
/// givers (data/quests.json) with location, distance, whether they're loaded (in spawn range), what
/// they want (turn-in items), and reward XP. Combine with /v1/observe/entities + /v1/move/goto
/// to walk to a giver and /v1/interact/give to hand in. See docs/autonomous-play.md.
async fn get_givers(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let player = s.player_info.lock().unwrap().clone();
    let zone = player.zone.clone();
    let (px, py) = (player.pos_east, player.pos_north);
    let live: HashMap<String, (f32, f32, f32)> = s.entity_positions.lock().unwrap().iter()
        .map(|(k, v)| (clean_entity_name(k), *v))
        .collect();
    let mut givers: Vec<serde_json::Value> = crate::quests::givers_in(&zone).into_iter()
        .map(|(name, g)| {
            let live_pos = live.get(&name).copied();
            let pos = live_pos.map(|(x, y, z)| [x, y, z]).unwrap_or([g.x, g.y, g.z]);
            let dist = ((pos[0] - px).powi(2) + (pos[1] - py).powi(2)).sqrt();
            serde_json::json!({
                "name": name,
                "npc_id": g.npc_id,
                "pos": pos,
                "loaded": live_pos.is_some(),
                "distance": dist.round(),
                "turn_in": g.turn_in,
                "wanted": g.wanted,
                "reward_xp": g.reward_xp,
                "hail": g.hail,
            })
        })
        .collect();
    givers.sort_by(|a, b| {
        let (da, db) = (a["distance"].as_f64().unwrap_or(1e9), b["distance"].as_f64().unwrap_or(1e9));
        da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
    });
    Json(serde_json::json!({ "zone": zone, "player": [px, py], "count": givers.len(), "quest_givers": givers }))
}

/// GET /v1/quests/log — the player's NATIVE quest journal (EQ Task system), pushed by the server
/// via OP_TaskDescription/OP_TaskActivity. Excludes Completed/Cancelled tasks — see
/// GET /v1/quests/completed for finished ones. Each task has a title, description, coin/XP/item
/// reward, and objectives with live progress (done_count/goal_count).
async fn get_log(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let tasks: Vec<_> = s.task_log.lock().unwrap().iter()
        .filter(|t| t.status == crate::game_state::TaskStatus::Active)
        .cloned()
        .collect();
    Json(serde_json::json!({ "active_count": tasks.len(), "tasks": tasks }))
}

/// GET /v1/quests/completed — completed task history: {task_id, title, completed_time}[].
async fn get_completed(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let completed = s.completed_tasks_shared.lock().unwrap().clone();
    Json(serde_json::json!({ "count": completed.len(), "completed": completed }))
}

/// GET /v1/quests/offers — pending task offers from an open selector window (OP_TaskSelectWindow):
/// {task_id, npc_id, title, description, has_rewards}[]. Empty unless an NPC is actively presenting
/// a choice of tasks (rare — most content auto-grants via assigntask, see GET /v1/quests/log).
async fn get_offers(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let offers = s.task_offers_shared.lock().unwrap().clone();
    Json(serde_json::json!({ "count": offers.len(), "offers": offers }))
}

#[derive(serde::Deserialize)]
struct TaskIdBody { task_id: u32 }

/// POST /v1/quests/accept {"task_id":N} — accept one offered task from an open selector window.
/// 400 if task_id isn't in the current GET /v1/quests/offers list.
async fn post_accept(
    State(s): State<HttpState>,
    body: Result<Json<TaskIdBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let task_id = match body {
        Ok(Json(b)) => b.task_id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"task_id\":N}".into()),
    };
    let known = s.task_offers_shared.lock().unwrap().iter().any(|o| o.task_id == task_id);
    if !known {
        return (StatusCode::BAD_REQUEST, format!("no pending task offer with task_id={task_id}"));
    }
    *s.accept_task.lock().unwrap() = Some(task_id);
    tracing::info!("quests: queued accept task_id={task_id}");
    (StatusCode::OK, format!("accepting task_id={task_id}"))
}

/// POST /v1/quests/decline — decline all pending task offers (idempotent no-op if none are open).
async fn post_decline(State(s): State<HttpState>) -> (StatusCode, String) {
    if s.task_offers_shared.lock().unwrap().is_empty() {
        return (StatusCode::OK, "no pending task offers".into());
    }
    *s.accept_task.lock().unwrap() = Some(0);
    tracing::info!("quests: queued decline-all");
    (StatusCode::OK, "declining pending task offer(s)".into())
}

/// POST /v1/quests/cancel {"task_id":N} — abandon an active task. 400 if task_id isn't in the
/// current journal (GET /v1/quests/log), since a missing entry means there's no sequence_number to
/// address the OP_CancelTask packet with.
async fn post_cancel(
    State(s): State<HttpState>,
    body: Result<Json<TaskIdBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let task_id = match body {
        Ok(Json(b)) => b.task_id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"task_id\":N}".into()),
    };
    let known = s.task_log.lock().unwrap().iter().any(|t| t.task_id == task_id);
    if !known {
        return (StatusCode::BAD_REQUEST, format!("no active task with task_id={task_id}"));
    }
    *s.cancel_task.lock().unwrap() = Some(task_id);
    tracing::info!("quests: queued cancel task_id={task_id}");
    (StatusCode::OK, format!("cancelling task_id={task_id}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    pub(crate) fn empty_state() -> HttpState {
        HttpState {
            cmd_tx: Arc::new(Mutex::new(None)),
            snapshot: Arc::new(Mutex::new(crate::camera_state::CameraSnapshot {
                mode: crate::camera_state::CameraMode::AutoFollow,
                azimuth: 0.0,
                elevation: 0.0,
                radius: 0.0,
                focus: [0.0, 0.0, 0.0],
            })),
            frame_req: Arc::new(Mutex::new(None)),
            goto_target: Arc::new(Mutex::new(None)),
            goto_entity: Arc::new(Mutex::new(None)),
            entity_positions: Arc::new(Mutex::new(HashMap::new())),
            entity_ids: Arc::new(Mutex::new(HashMap::new())),
            zone_points: Arc::new(Mutex::new(Vec::new())),
            zone_cross: Arc::new(Mutex::new(None)),
            hail: Arc::new(Mutex::new(None)),
            say: Arc::new(Mutex::new(None)),
            target: Arc::new(Mutex::new(None)),
            attack: Arc::new(Mutex::new(None)),
            cast: Arc::new(Mutex::new(None)),
            mem_spell: Arc::new(Mutex::new(None)),
            sit: Arc::new(Mutex::new(None)),
            consider: Arc::new(Mutex::new(None)),
            buy: Arc::new(Mutex::new(None)),
            sell: Arc::new(Mutex::new(None)),
            trade: Arc::new(Mutex::new(None)),
            merchant: Arc::new(Mutex::new(MerchantSnapshot::default())),
            move_req: Arc::new(Mutex::new(None)),
            give: Arc::new(Mutex::new(None)),
            inventory: Arc::new(Mutex::new(Vec::new())),
            loot: Arc::new(Mutex::new(None)),
            messages: Arc::new(Mutex::new(Vec::new())),
            chat_events: Arc::new(Mutex::new(Vec::new())),
            chat_send: Arc::new(Mutex::new(Vec::new())),
            spells: std::sync::Arc::new(crate::spells::SpellDb::default()),
            player_info: Arc::new(Mutex::new(PlayerState::default())),
            task_log: Arc::new(Mutex::new(Vec::new())),
            task_offers_shared: Arc::new(Mutex::new(Vec::new())),
            completed_tasks_shared: Arc::new(Mutex::new(Vec::new())),
            accept_task: Arc::new(Mutex::new(None)),
            cancel_task: Arc::new(Mutex::new(None)),
            door_click: Arc::new(Mutex::new(None)),
            doors_shared: Arc::new(Mutex::new(Vec::new())),
            camp: Arc::new(Mutex::new(None)),
            camp_until: Arc::new(Mutex::new(None)),
            group:             Arc::new(Mutex::new(GroupSnapshot::default())),
            group_invite:      Arc::new(Mutex::new(None)),
            group_accept:      Arc::new(Mutex::new(None)),
            group_decline:     Arc::new(Mutex::new(None)),
            group_leave:       Arc::new(Mutex::new(None)),
            group_kick:        Arc::new(Mutex::new(None)),
            group_make_leader: Arc::new(Mutex::new(None)),
        }
    }

    #[tokio::test]
    async fn accept_unknown_task_id_is_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/accept")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_id":999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accept_known_offer_is_200_and_queues_request() {
        let state = empty_state();
        state.task_offers_shared.lock().unwrap().push(crate::game_state::TaskOffer {
            task_id: 42, npc_id: 7, title: "Offer".into(), description: String::new(), has_rewards: false,
        });
        let accept = state.accept_task.clone();
        let app = router().with_state(state);
        let req = Request::post("/accept")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_id":42}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*accept.lock().unwrap(), Some(42));
    }

    #[tokio::test]
    async fn decline_with_no_offers_is_idempotent_200() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/decline").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn cancel_unknown_task_id_is_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/cancel")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_id":999}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn cancel_known_task_is_200_and_queues_request() {
        let state = empty_state();
        state.task_log.lock().unwrap().push(crate::game_state::ActiveTask {
            task_id: 42, sequence_number: 3, ..Default::default()
        });
        let cancel = state.cancel_task.clone();
        let app = router().with_state(state);
        let req = Request::post("/cancel")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_id":42}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(*cancel.lock().unwrap(), Some(42));
    }

    #[tokio::test]
    async fn log_filters_out_completed_tasks() {
        let state = empty_state();
        state.task_log.lock().unwrap().extend([
            crate::game_state::ActiveTask { task_id: 1, status: crate::game_state::TaskStatus::Active, ..Default::default() },
            crate::game_state::ActiveTask { task_id: 2, status: crate::game_state::TaskStatus::Completed, ..Default::default() },
        ]);
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/log").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active_count"], 1);
    }
}
