//! `/v1/trainer/*` — guildmaster skill training (eqoxide#99). Mirrors the merchant flow: `open` a
//! trainer window, `list` the skills it will train (and their cost basis = current→cap), then
//! `train` one point of a skill. Backed by OP_GMTraining / OP_GMTrainSkill (see the builders in
//! `navigation.rs` and the handlers in `packet_handler.rs`).

use axum::{routing::{get, post}, extract::State, Json, http::StatusCode, Router};
use crate::{HttpState, clean_entity_name, require_live_session};
use crate::refusal::Refusal;

pub fn router() -> Router<HttpState> {
    Router::new()
        .route("/open",  post(post_open))
        .route("/list",  get(get_list))
        .route("/train", post(post_train))
        .route("/close", post(post_close))
}

// ── 409 CONFLICT bodies for an occupied command slot (#347 step 2) ───────────────────────────────
// Each `/v1/trainer/*` verb queues into a single-slot mailbox the net thread drains once per tick.
// Before #347 a second request inside that window OVERWROTE the pending one and BOTH callers were
// told `200`, so one of the two actions silently never happened. The slot now refuses the second
// write and keeps the first. A 409 here means the request was NOT queued and definitively did not
// happen — retrying after the drain is safe. `open` and `close` share one slot (close is `open(0)`),
// so either can 409 the other.
const BUSY_TRAINER: &str = "a trainer open/close is already queued and undrained — retry in a moment (it was NOT queued)";
const BUSY_TRAIN: &str = "a train request is already queued and undrained — retry in a moment (it was NOT queued)";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct OpenBody { name: Option<String>, trainer: Option<String> }

/// POST /v1/trainer/open {"trainer":"Name"} — open the GM-skills window with a (fuzzily-named) nearby
/// guildmaster. Resolves the name to a spawn id and sends OP_GMTraining; the server replies with the
/// offered caps, which populate GET /v1/trainer/list.
async fn post_open(
    State(s): State<HttpState>,
    body: Result<Json<OpenBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    // #952/#956 (agent-honesty): `trainer` and `name` are two spellings of ONE trainer, and
    // `.or` is a precedence chain — `{"name":"Alpha","trainer":"Beta"}` used to answer
    // `200 opening training with Beta`, silently discarding Alpha. Two names are two different
    // NPCs, so the request is refused rather than resolved to one of them. Destructured
    // exhaustively (no `..`) so a new field on `OpenBody` cannot slip past this decision.
    let name = match body {
        Ok(Json(b)) => {
            let OpenBody { name, trainer } = &b;
            if let Some(msg) = crate::req_form::conflicting_forms(
                "guildmaster selection", &[("name", name.is_some()), ("trainer", trainer.is_some())],
            ) {
                return (StatusCode::BAD_REQUEST, msg);
            }
            b.trainer.or(b.name)
        }
        Err(_) => None,
    };
    let Some(name) = name else {
        return (StatusCode::BAD_REQUEST, "provide {\"trainer\":\"Name\"}".into());
    };
    let ids = s.world.entity_ids();
    let nl = name.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase() == nl)
        .or_else(|| ids.iter().find(|(k, _)| {
            clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl)
        }))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            if let Some(busy) = s.command.request_open_trainer(id).refused(BUSY_TRAINER) { return busy; }
            (StatusCode::OK, format!("opening training with {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no entity matching {:?}", name)),
    }
}

/// GET /v1/trainer/list — skills the open trainer will raise, as `{id, name, current, cap}`, listing
/// only skills where `cap > current` (i.e. actually trainable). `open:false` if no window is open.
async fn get_list(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let pi = s.player();
    if !pi.trainer_open {
        return Json(serde_json::json!({ "open": false, "skills": [] }));
    }
    let caps = pi.trainer_skills.clone();
    let cur  = pi.skills.clone();
    drop(pi);
    let list: Vec<_> = (0..eqoxide_core::skills::NUM_SKILLS).filter_map(|id| {
        let cap = caps.get(id).copied().unwrap_or(0);
        let current = cur.get(id).copied().unwrap_or(0);
        (cap > current).then(|| serde_json::json!({
            "id": id, "name": eqoxide_core::skills::skill_name(id as u32), "current": current, "cap": cap,
        }))
    }).collect();
    Json(serde_json::json!({ "open": true, "skills": list }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TrainBody { skill_id: u32 }

/// POST /v1/trainer/train {"skill_id":N} — train one point of a skill at the open trainer. The server
/// raises the skill (spending a skill point + coin) and echoes OP_SkillUpdate, which updates the
/// value seen by /v1/observe/skills and /v1/trainer/list. 400 if no window is open.
async fn post_train(
    State(s): State<HttpState>,
    body: Result<Json<TrainBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"skill_id\":N}".into()),
    };
    if !s.player().trainer_open {
        return (StatusCode::BAD_REQUEST, "no trainer window open — call /v1/trainer/open first".into());
    }
    if let Some(busy) = s.command.request_train_skill(b.skill_id).refused(BUSY_TRAIN) { return busy; }
    let name = eqoxide_core::skills::skill_name(b.skill_id).unwrap_or("?");
    (StatusCode::OK, format!("training {} (skill_id={})", name, b.skill_id))
}

/// POST /v1/trainer/close — end the open training session (OP_GMEndTraining). Uses the
/// `trainer_open_req` slot's `Some(0)` sentinel (0 is never a real spawn id) so no extra
/// request slot needs threading through the nav chain (#162).
async fn post_close(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if let Some(busy) = s.command.request_open_trainer(0).refused(BUSY_TRAINER) { return busy; }
    (StatusCode::OK, "closing trainer".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{empty_state, set_gs};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn post(path: &'static str) -> Request<Body> {
        Request::post(path).body(Body::empty()).unwrap()
    }

    fn train_req(skill_id: u32) -> Request<Body> {
        Request::post("/train")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"skill_id":{skill_id}}}"#)))
            .unwrap()
    }

    #[tokio::test]
    async fn close_is_200_and_queues_the_zero_sentinel() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(post("/close")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_trainer_open(), Some(0));
    }

    /// #347 step 2 (review round 1, B1): `open` and `close` share one slot, so a second window verb
    /// inside the same undrained tick used to OVERWRITE the first with both callers told `200`.
    /// `/close` is used for both shots because it needs no resolvable entity — the slot, not the
    /// name lookup, is what is under test.
    #[tokio::test]
    async fn a_second_trainer_window_verb_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);

        let first = app.clone().oneshot(post("/close")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(post("/close")).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second trainer window verb must be refused, not silently swallowed");

        assert_eq!(command.take_trainer_open(), Some(0),
            "the FIRST request must be the one that survives to the net thread");
        assert_eq!(command.take_trainer_open(), None,
            "and nothing else may be queued behind it");
    }

    #[tokio::test]
    async fn train_with_no_window_open_is_400_and_queues_nothing() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(train_req(1)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert_eq!(command.take_train_skill(), None);
    }

    /// The `train` slot is separate from the window slot, and carries the same #347 step 2 promise.
    #[tokio::test]
    async fn a_second_train_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        set_gs(&state, |gs| gs.trainer_open = Some(7));
        let command = state.command.clone();
        let app = router().with_state(state);

        let first = app.clone().oneshot(train_req(1)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(train_req(2)).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second train must be refused, not silently swallowed");

        assert_eq!(command.take_train_skill(), Some(1),
            "the FIRST train must be the one that survives to the net thread");
        assert_eq!(command.take_train_skill(), None,
            "and nothing else may be queued behind it");
    }
}
