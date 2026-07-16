//! `/v1/quests/*` — the native EQ Task-system journal (server-pushed quest log).

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/log", get(get_log))
        .route("/completed", get(get_completed))
        .route("/offers", get(get_offers))
        .route("/accept", post(post_accept))
        .route("/decline", post(post_decline))
        .route("/cancel", post(post_cancel))
}

/// GET /v1/quests/log — the player's NATIVE quest journal (EQ Task system), pushed by the server
/// via OP_TaskDescription/OP_TaskActivity. Excludes Completed/Cancelled tasks — see
/// GET /v1/quests/completed for finished ones. Each task has a title, description, coin/XP/item
/// reward, and objectives with live progress (done_count/goal_count).
async fn get_log(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let tasks: Vec<_> = s.quest.task_log.lock().unwrap().iter()
        .filter(|t| t.status == crate::game_state::TaskStatus::Active)
        .cloned()
        .collect();
    Json(serde_json::json!({ "active_count": tasks.len(), "tasks": tasks }))
}

/// GET /v1/quests/completed — completed task history: {task_id, title, completed_time}[].
async fn get_completed(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let completed = s.quest.completed_tasks_shared.lock().unwrap().clone();
    Json(serde_json::json!({ "count": completed.len(), "completed": completed }))
}

/// GET /v1/quests/offers — pending task offers from an open selector window (OP_TaskSelectWindow):
/// {task_id, npc_id, title, description, has_rewards}[]. Empty unless an NPC is actively presenting
/// a choice of tasks (rare — most content auto-grants via assigntask, see GET /v1/quests/log).
async fn get_offers(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let offers = s.quest.task_offers_shared.lock().unwrap().clone();
    Json(serde_json::json!({ "count": offers.len(), "offers": offers }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
    let known = s.quest.task_offers_shared.lock().unwrap().iter().any(|o| o.task_id == task_id);
    if !known {
        return (StatusCode::BAD_REQUEST, format!("no pending task offer with task_id={task_id}"));
    }
    s.command.request_accept_task(task_id);
    tracing::info!("quests: queued accept task_id={task_id}");
    (StatusCode::OK, format!("accepting task_id={task_id}"))
}

/// POST /v1/quests/decline — decline all pending task offers (idempotent no-op if none are open).
async fn post_decline(State(s): State<HttpState>) -> (StatusCode, String) {
    if s.quest.task_offers_shared.lock().unwrap().is_empty() {
        return (StatusCode::OK, "no pending task offers".into());
    }
    s.command.request_accept_task(0);
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
    let known = s.quest.task_log.lock().unwrap().iter().any(|t| t.task_id == task_id);
    if !known {
        return (StatusCode::BAD_REQUEST, format!("no active task with task_id={task_id}"));
    }
    s.command.request_cancel_task(task_id);
    tracing::info!("quests: queued cancel task_id={task_id}");
    (StatusCode::OK, format!("cancelling task_id={task_id}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    /// An `Instant` `secs` in the past (saturating — a just-booted host can't go below its epoch).
    pub(crate) fn ago(secs: u64) -> std::time::Instant {
        std::time::Instant::now()
            .checked_sub(std::time::Duration::from_secs(secs))
            .expect("monotonic clock older than the test window")
    }

    /// Mutate the network thread's published `GameState` — the single source of truth every
    /// agent-facing player field is projected from (#343). Tests that used to poke `player_info`
    /// directly now seed the snapshot the network thread would have published.
    pub(crate) fn set_gs(state: &HttpState, f: impl FnOnce(&mut crate::game_state::GameState)) {
        let mut gs = (**state.game_state.load()).clone();
        f(&mut gs);
        state.game_state.store(Arc::new(gs));
    }

    pub(crate) fn empty_state() -> HttpState {
        HttpState {
            // `CameraSlots` has no `Default` impl (`CameraSnapshot`'s fields aren't Default-able),
            // so it's the one bundle built by hand here; every other bundle is plain `Default::default()`.
            camera: crate::ipc::CameraSlots {
                cmd_tx: Arc::new(Mutex::new(None)),
                snapshot: Arc::new(Mutex::new(crate::camera_state::CameraSnapshot {
                    mode: crate::camera_state::CameraMode::AutoFollow,
                    azimuth: 0.0,
                    elevation: 0.0,
                    radius: 0.0,
                    focus: [0.0, 0.0, 0.0],
                })),
                frame_req: Arc::new(Mutex::new(None)),
                manual_move: Arc::new(Mutex::new(None)),
            },
            nav: Default::default(),
            world: Default::default(),
            shared_collision: Arc::new(std::sync::RwLock::new(None)),
            command: Default::default(),
            social: Default::default(),
            merchant_slots: Default::default(),
            inventory_slots: Default::default(),
            interact: Default::default(),
            chat: Default::default(),
            spells: std::sync::Arc::new(crate::spells::SpellDb::default()),
            game_state: Arc::new(arc_swap::ArcSwap::from_pointee(crate::game_state::GameState::new())),
            net_health: Arc::new(Mutex::new(crate::http::NetHealth::default())),
            frame_profile: Arc::new(Mutex::new(crate::profiling::FrameProfile::default())),
            quest: Default::default(),
            group_slots: Default::default(),
            lifecycle: Default::default(),
            guild_slots: Default::default(),
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
        state.quest.task_offers_shared.lock().unwrap().push(crate::game_state::TaskOffer {
            task_id: 42, npc_id: 7, title: "Offer".into(), description: String::new(), has_rewards: false,
        });
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/accept")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_id":42}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_accept_task(), Some(42));
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
        state.quest.task_log.lock().unwrap().push(crate::game_state::ActiveTask {
            task_id: 42, sequence_number: 3, ..Default::default()
        });
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/cancel")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"task_id":42}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_cancel_task(), Some(42));
    }

    #[tokio::test]
    async fn log_filters_out_completed_tasks() {
        let state = empty_state();
        state.quest.task_log.lock().unwrap().extend([
            crate::game_state::ActiveTask { task_id: 1, status: crate::game_state::TaskStatus::Active, ..Default::default() },
            crate::game_state::ActiveTask { task_id: 2, status: crate::game_state::TaskStatus::Completed, ..Default::default() },
        ]);
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/log").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["active_count"], 1);
        // Assert WHICH task survives the filter, not just the count — inverting the filter
        // (status == Active -> != Active) would also yield a count of 1 (task_id 2), but with
        // the wrong task. See #355 M3.
        assert_eq!(json["tasks"][0]["task_id"], 1);
    }
}
