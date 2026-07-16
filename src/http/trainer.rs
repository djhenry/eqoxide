//! `/v1/trainer/*` — guildmaster skill training (eqoxide#99). Mirrors the merchant flow: `open` a
//! trainer window, `list` the skills it will train (and their cost basis = current→cap), then
//! `train` one point of a skill. Backed by OP_GMTraining / OP_GMTrainSkill (see the builders in
//! `navigation.rs` and the handlers in `packet_handler.rs`).

use axum::{routing::{get, post}, extract::State, Json, http::StatusCode, Router};
use crate::http::{HttpState, clean_entity_name};

pub fn router() -> Router<HttpState> {
    Router::new()
        .route("/open",  post(post_open))
        .route("/list",  get(get_list))
        .route("/train", post(post_train))
        .route("/close", post(post_close))
}

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
    let name = match body {
        Ok(Json(b)) => b.trainer.or(b.name),
        Err(_) => None,
    };
    let Some(name) = name else {
        return (StatusCode::BAD_REQUEST, "provide {\"trainer\":\"Name\"}".into());
    };
    let ids = s.world.entity_ids.lock().unwrap();
    let nl = name.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase() == nl)
        .or_else(|| ids.iter().find(|(k, _)| {
            clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl)
        }))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            s.command.request_open_trainer(id);
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
    let list: Vec<_> = (0..crate::skills::NUM_SKILLS).filter_map(|id| {
        let cap = caps.get(id).copied().unwrap_or(0);
        let current = cur.get(id).copied().unwrap_or(0);
        (cap > current).then(|| serde_json::json!({
            "id": id, "name": crate::skills::skill_name(id as u32), "current": current, "cap": cap,
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
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"skill_id\":N}".into()),
    };
    if !s.player().trainer_open {
        return (StatusCode::BAD_REQUEST, "no trainer window open — call /v1/trainer/open first".into());
    }
    s.command.request_train_skill(b.skill_id);
    let name = crate::skills::skill_name(b.skill_id).unwrap_or("?");
    (StatusCode::OK, format!("training {} (skill_id={})", name, b.skill_id))
}

/// POST /v1/trainer/close — end the open training session (OP_GMEndTraining). Uses the
/// `trainer_open_req` slot's `Some(0)` sentinel (0 is never a real spawn id) so no extra
/// request slot needs threading through the nav chain (#162).
async fn post_close(State(s): State<HttpState>) -> (StatusCode, String) {
    s.command.request_open_trainer(0);
    (StatusCode::OK, "closing trainer".into())
}
