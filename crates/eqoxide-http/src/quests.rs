//! `/v1/quests/*` — the native EQ Task-system journal (server-pushed quest log).

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;
use crate::refusal::Refusal;

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
        .filter(|t| t.status == eqoxide_core::game_state::TaskStatus::Active)
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

// ── 409 CONFLICT bodies for an occupied command slot (#347 step 2) ──────────────────────────
// Each `/v1/quests/*` verb queues into a single-slot mailbox the net thread drains once per tick.
// Before #347 a second request inside that window OVERWROTE the pending one and BOTH callers were
// told `200`, so one of the two actions silently never happened. The slot now refuses the second
// write and keeps the first. A 409 here means the request was NOT queued and definitively did not
// happen — retrying after the drain is safe. `accept` and `decline` share a slot (a decline is an
// accept of task 0), so either can 409 the other.
const BUSY_TASK: &str = "a task accept/decline is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_CANCEL: &str = "a task cancel is already queued and undrained — retry in a moment (it was NOT queued)";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskIdBody { task_id: u32 }

/// POST /v1/quests/accept {"task_id":N} — accept one offered task from an open selector window.
/// 400 if task_id isn't in the current GET /v1/quests/offers list.
async fn post_accept(
    State(s): State<HttpState>,
    body: Result<Json<TaskIdBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let task_id = match body {
        Ok(Json(b)) => b.task_id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"task_id\":N}".into()),
    };
    let known = s.quest.task_offers_shared.lock().unwrap().iter().any(|o| o.task_id == task_id);
    if !known {
        return (StatusCode::BAD_REQUEST, format!("no pending task offer with task_id={task_id}"));
    }
    if let Some(busy) = s.command.request_accept_task(task_id).refused(BUSY_TASK) { return busy; }
    tracing::info!("quests: queued accept task_id={task_id}");
    (StatusCode::OK, format!("accepting task_id={task_id}"))
}

/// POST /v1/quests/decline — decline all pending task offers (idempotent no-op if none are open).
async fn post_decline(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if s.quest.task_offers_shared.lock().unwrap().is_empty() {
        return (StatusCode::OK, "no pending task offers".into());
    }
    if let Some(busy) = s.command.request_accept_task(0).refused(BUSY_TASK) { return busy; }
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
    if let Err(e) = require_live_session(&s) { return e; }
    let task_id = match body {
        Ok(Json(b)) => b.task_id,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"task_id\":N}".into()),
    };
    let known = s.quest.task_log.lock().unwrap().iter().any(|t| t.task_id == task_id);
    if !known {
        return (StatusCode::BAD_REQUEST, format!("no active task with task_id={task_id}"));
    }
    if let Some(busy) = s.command.request_cancel_task(task_id).refused(BUSY_CANCEL) { return busy; }
    tracing::info!("quests: queued cancel task_id={task_id}");
    (StatusCode::OK, format!("cancelling task_id={task_id}"))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::testkit::empty_state;

    fn accept_req(task_id: u32) -> Request<Body> {
        Request::post("/accept")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"task_id":{task_id}}}"#)))
            .unwrap()
    }

    fn cancel_req(task_id: u32) -> Request<Body> {
        Request::post("/cancel")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"task_id":{task_id}}}"#)))
            .unwrap()
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
        state.quest.task_offers_shared.lock().unwrap().push(eqoxide_core::game_state::TaskOffer {
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
        state.quest.task_log.lock().unwrap().push(eqoxide_core::game_state::ActiveTask {
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
            eqoxide_core::game_state::ActiveTask { task_id: 1, status: eqoxide_core::game_state::TaskStatus::Active, ..Default::default() },
            eqoxide_core::game_state::ActiveTask { task_id: 2, status: eqoxide_core::game_state::TaskStatus::Completed, ..Default::default() },
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

    /// #347 step 2 (review round 1, B1): two accepts inside one undrained tick. Before the fix the
    /// second OVERWROTE the first and BOTH callers were told `200`, so one of the two tasks was
    /// never accepted while the agent believed both were.
    #[tokio::test]
    async fn a_second_accept_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        state.quest.task_offers_shared.lock().unwrap().extend([
            eqoxide_core::game_state::TaskOffer {
                task_id: 42, npc_id: 7, title: "First".into(), description: String::new(), has_rewards: false,
            },
            eqoxide_core::game_state::TaskOffer {
                task_id: 43, npc_id: 7, title: "Second".into(), description: String::new(), has_rewards: false,
            },
        ]);
        let command = state.command.clone();
        let app = router().with_state(state);

        let first = app.clone().oneshot(accept_req(42)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(accept_req(43)).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second accept must be refused, not silently swallowed");

        assert_eq!(command.take_accept_task(), Some(42),
            "the FIRST accept must be the one that survives to the net thread");
        assert_eq!(command.take_accept_task(), None,
            "and nothing else may be queued behind it");
    }

    /// Same promise on the `cancel` slot, which is a different mailbox from `accept`.
    #[tokio::test]
    async fn a_second_cancel_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        state.quest.task_log.lock().unwrap().extend([
            eqoxide_core::game_state::ActiveTask { task_id: 42, sequence_number: 3, ..Default::default() },
            eqoxide_core::game_state::ActiveTask { task_id: 43, sequence_number: 4, ..Default::default() },
        ]);
        let command = state.command.clone();
        let app = router().with_state(state);

        let first = app.clone().oneshot(cancel_req(42)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(cancel_req(43)).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second cancel must be refused, not silently swallowed");

        assert_eq!(command.take_cancel_task(), Some(42),
            "the FIRST cancel must be the one that survives to the net thread");
        assert_eq!(command.take_cancel_task(), None,
            "and nothing else may be queued behind it");
    }
}
