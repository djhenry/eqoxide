//! `/v1/group/*` — group (party) management: invite, accept, decline, leave, kick, makeleader,
//! and the live roster view.

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/roster", get(get_roster))
        .route("/invite", post(post_invite))
        .route("/accept", post(post_accept))
        .route("/decline", post(post_decline))
        .route("/leave", post(post_leave))
        .route("/kick", post(post_kick))
        .route("/makeleader", post(post_makeleader))
}

/// GET /v1/group/roster — current group roster, leader, and any pending invite. Empty
/// `members` means not currently grouped.
async fn get_roster(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let g = s.group_slots.group.lock().unwrap();
    Json(serde_json::json!({
        "leader": g.leader,
        "pending_invite": g.pending_invite,
        "you_are_leader": g.you_are_leader,
        "members": g.members,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NameBody { name: String }

fn extract_name(body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>) -> Result<String, (StatusCode, String)> {
    match body {
        Ok(Json(b)) if !b.name.trim().is_empty() => Ok(b.name),
        _ => Err((StatusCode::BAD_REQUEST, "provide {\"name\":\"X\"}".into())),
    }
}

/// POST /v1/group/invite {"name":"X"} — send an invite to player X.
async fn post_invite(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    s.command.request_group_invite(name.clone());
    tracing::info!("group: queued invite to {name}");
    (StatusCode::OK, format!("inviting {name}"))
}

/// POST /v1/group/accept — accept the current pending invite. 400 if there is none.
async fn post_accept(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if s.group_slots.group.lock().unwrap().pending_invite.is_none() {
        return (StatusCode::BAD_REQUEST, "no pending invite".into());
    }
    s.command.request_group_accept();
    tracing::info!("group: queued accept");
    (StatusCode::OK, "accepting invite".into())
}

/// POST /v1/group/decline — decline the current pending invite (sends a defensive
/// OP_GroupDisband cleanup — RoF2 has no working OP_GroupCancelInvite). 400 if there is none.
async fn post_decline(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if s.group_slots.group.lock().unwrap().pending_invite.is_none() {
        return (StatusCode::BAD_REQUEST, "no pending invite".into());
    }
    s.command.request_group_decline();
    tracing::info!("group: queued decline");
    (StatusCode::OK, "declining invite".into())
}

/// POST /v1/group/leave — leave the current group. If leader with < 3 total members, this fully
/// disbands the group (confirmed EQEmu server behavior — no auto handoff). 400 if not grouped.
async fn post_leave(State(s): State<HttpState>) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    if s.group_slots.group.lock().unwrap().members.is_empty() {
        return (StatusCode::BAD_REQUEST, "not in a group".into());
    }
    s.command.request_group_leave();
    tracing::info!("group: queued leave");
    (StatusCode::OK, "leaving group".into())
}

/// POST /v1/group/kick {"name":"X"} — leader-only: remove member X. 400 if not leader or X isn't
/// a current member.
async fn post_kick(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    let g = s.group_slots.group.lock().unwrap();
    if !g.you_are_leader {
        return (StatusCode::BAD_REQUEST, "only the group leader can kick".into());
    }
    if !g.members.iter().any(|m| m.name == name) {
        return (StatusCode::BAD_REQUEST, format!("{name} is not a current group member"));
    }
    drop(g);
    s.command.request_group_kick(name.clone());
    tracing::info!("group: queued kick of {name}");
    (StatusCode::OK, format!("kicking {name}"))
}

/// POST /v1/group/makeleader {"name":"X"} — leader-only: transfer leadership to X without
/// disbanding. 400 if not leader or X isn't a current member.
async fn post_makeleader(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    let g = s.group_slots.group.lock().unwrap();
    if !g.you_are_leader {
        return (StatusCode::BAD_REQUEST, "only the group leader can transfer leadership".into());
    }
    if !g.members.iter().any(|m| m.name == name) {
        return (StatusCode::BAD_REQUEST, format!("{name} is not a current group member"));
    }
    drop(g);
    s.command.request_group_make_leader(name.clone());
    tracing::info!("group: queued makeleader {name}");
    (StatusCode::OK, format!("transferring leadership to {name}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    fn empty_state() -> HttpState {
        crate::testkit::empty_state()
    }

    #[tokio::test]
    async fn roster_is_empty_when_not_grouped() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::get("/roster").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["members"].as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn invite_with_empty_name_is_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let req = Request::post("/invite")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":""}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn invite_with_name_is_200_and_queues_request() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/invite")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sariel"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_group_invite(), Some("Sariel".to_string()));
    }

    #[tokio::test]
    async fn accept_with_no_pending_invite_is_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/accept").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn accept_with_pending_invite_is_200_and_queues_request() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().pending_invite = Some("Sariel".into());
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/accept").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(command.take_group_accept().is_some());
    }

    #[tokio::test]
    async fn decline_with_no_pending_invite_is_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/decline").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn leave_when_not_grouped_is_400() {
        let state = empty_state();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/leave").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn leave_when_grouped_is_200_and_queues_request() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().members.push(GroupMemberView {
            name: "Aldric".into(), level: 10, is_leader: true, is_merc: false,
            tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
        });
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(Request::post("/leave").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert!(command.take_group_leave().is_some());
    }

    #[tokio::test]
    async fn kick_when_not_leader_is_400() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().members.push(GroupMemberView {
            name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
            tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
        });
        state.group_slots.group.lock().unwrap().you_are_leader = false;
        let app = router().with_state(state);
        let req = Request::post("/kick")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sariel"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn kick_unknown_member_is_400() {
        let state = empty_state();
        state.group_slots.group.lock().unwrap().you_are_leader = true;
        let app = router().with_state(state);
        let req = Request::post("/kick")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Nobody"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn kick_known_member_as_leader_is_200_and_queues_request() {
        let state = empty_state();
        {
            let mut g = state.group_slots.group.lock().unwrap();
            g.you_are_leader = true;
            g.members.push(GroupMemberView {
                name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
                tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
            });
        }
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/kick")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sariel"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_group_kick(), Some("Sariel".to_string()));
    }

    #[tokio::test]
    async fn makeleader_when_not_leader_is_400() {
        let state = empty_state();
        {
            // Seed the target as a current member so the membership guard is satisfied and
            // the LEADER guard is the one actually under test here — without this, the 400
            // comes from "Sariel is not a current group member" instead. See #355 M4.
            let mut g = state.group_slots.group.lock().unwrap();
            g.you_are_leader = false;
            g.members.push(GroupMemberView {
                name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
                tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
            });
        }
        let app = router().with_state(state);
        let req = Request::post("/makeleader")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sariel"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], b"only the group leader can transfer leadership");
    }

    #[tokio::test]
    async fn makeleader_known_member_as_leader_is_200_and_queues_request() {
        let state = empty_state();
        {
            let mut g = state.group_slots.group.lock().unwrap();
            g.you_are_leader = true;
            g.members.push(GroupMemberView {
                name: "Sariel".into(), level: 8, is_leader: false, is_merc: false,
                tank: false, assist: false, puller: false, offline: false, hp_pct: 100.0,
            });
        }
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/makeleader")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"name":"Sariel"}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_group_make_leader(), Some("Sariel".to_string()));
    }
}
