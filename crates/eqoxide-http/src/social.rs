//! `/v1/social/*` — the client-local friends/presence list (#301). Friends are stored client-side
//! (nothing goes over the wire on add/remove, matching the real RoF2 client's `[Friends]` ini);
//! presence is a pull: a poll sends OP_FriendsWho and the server replies (as OP_WhoAllResponse) with
//! the online subset.

use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
use tokio::sync::oneshot;
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/friends", get(get_friends).post(post_friends))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct FriendsBody {
    /// Name to add to the friends list.
    add:    Option<String>,
    /// Name to remove from the friends list.
    remove: Option<String>,
}

/// POST /v1/social/friends {"add":"Name"} or {"remove":"Name"} — edit the client-local friends list
/// (case-insensitive de-dupe on add; case-insensitive match on remove). No packet is sent.
async fn post_friends(
    State(s): State<HttpState>,
    body: Result<Json<FriendsBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"add\":\"Name\"} or {\"remove\":\"Name\"}".into()),
    };
    // #952/#956 (agent-honesty): `add` and `remove` are opposite edits to one list, and the
    // `if`/`else if` below is a precedence chain — `{"add":"Alpha","remove":"Beta"}` used to answer
    // `200 added Alpha` while Beta stayed on the list, with nothing in the response saying the
    // removal had not happened. An agent that batches both edits into one call therefore believed a
    // removal it never got. Refused instead: send two requests.
    //
    // Presence here is the handler's OWN notion of "supplied" — a non-empty trimmed name, the same
    // predicate the two branches use — so a request that already worked (an empty `add` beside a
    // real `remove`) keeps working. Destructured exhaustively (no `..`).
    let FriendsBody { add, remove } = &b;
    let supplied = |v: &Option<String>| v.as_ref().is_some_and(|n| !n.trim().is_empty());
    if let Some(msg) = crate::req_form::conflicting_forms(
        "friends-list edit", &[("add", supplied(add)), ("remove", supplied(remove))],
    ) {
        return (StatusCode::BAD_REQUEST, msg);
    }
    let mut list = s.social.friends_list.lock().unwrap();
    if let Some(name) = b.add.as_ref().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        if name.len() >= 64 {
            return (StatusCode::BAD_REQUEST, "friend name too long (max 63 chars — the server drops the whole reply otherwise)".into());
        }
        if !list.iter().any(|f| f.eq_ignore_ascii_case(name)) {
            list.push(name.to_string());
        }
        (StatusCode::OK, format!("added {name}"))
    } else if let Some(name) = b.remove.as_ref().map(|n| n.trim()).filter(|n| !n.is_empty()) {
        let before = list.len();
        list.retain(|f| !f.eq_ignore_ascii_case(name));
        if list.len() == before { (StatusCode::NOT_FOUND, format!("{name} was not in the friends list")) }
        else { (StatusCode::OK, format!("removed {name}")) }
    } else {
        (StatusCode::BAD_REQUEST, "provide a non-empty {\"add\":\"Name\"} or {\"remove\":\"Name\"}".into())
    }
}

#[derive(serde::Serialize)]
struct FriendView {
    name:   String,
    online: bool,
    /// Populated only for online friends (from the OP_FriendsWho reply): where they are + who they are.
    #[serde(skip_serializing_if = "Option::is_none")]
    zone_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    level:   Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    class:   Option<String>,
}

/// GET /v1/social/friends — the friends list with live online status. Triggers an OP_FriendsWho poll
/// and awaits the reply (the online subset), then annotates the full client-local list: a friend is
/// `online` iff the server returned it. 503 if not connected / no reply in time.
async fn get_friends(State(s): State<HttpState>) -> (StatusCode, Json<serde_json::Value>) {
    let friends = s.social.friends_list.lock().unwrap().clone();
    if friends.is_empty() {
        return (StatusCode::OK, Json(serde_json::json!({ "friends": [] })));
    }
    let (tx, rx) = oneshot::channel::<Vec<eqoxide_core::game_state::WhoEntry>>();
    s.command.request_friends_who(tx);
    match tokio::time::timeout(std::time::Duration::from_secs(6), rx).await {
        Ok(Ok(online_roster)) => {
            // Index the online subset by lowercased name for annotation.
            let online: std::collections::HashMap<String, eqoxide_core::game_state::WhoEntry> =
                online_roster.into_iter().map(|e| (e.name.to_lowercase(), e)).collect();
            let list: Vec<FriendView> = friends.into_iter().map(|name| {
                match online.get(&name.to_lowercase()) {
                    Some(e) => FriendView {
                        name, online: true,
                        zone_id: Some(e.zone_id),
                        level:   if e.anon { None } else { Some(e.level) },
                        class:   if e.anon { None } else { Some(eqoxide_core::race_class::class_name(e.class).to_string()) },
                    },
                    None => FriendView { name, online: false, zone_id: None, level: None, class: None },
                }
            }).collect();
            (StatusCode::OK, Json(serde_json::json!({ "friends": list })))
        }
        _ => (StatusCode::SERVICE_UNAVAILABLE,
              Json(serde_json::json!({ "error": "no OP_FriendsWho reply (not connected, or server did not reply in time)" }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::empty_state;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn body_text(resp: axum::response::Response) -> String {
        let b = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8_lossy(&b).into_owned()
    }

    fn friends_req(json: &str) -> Request<Body> {
        Request::post("/friends")
            .header("content-type", "application/json")
            .body(Body::from(json.to_string()))
            .unwrap()
    }

    /// #952/#956, the EXEMPTION this route's presence predicate exists for.
    ///
    /// `post_friends` is the one new conflict check that does not use `is_some()`: it asks whether
    /// the name is a non-empty trimmed string, which is the same predicate the two edit branches
    /// themselves use. Without that, a client that always emits both keys and leaves the unused one
    /// blank — `{"add":"","remove":"Beta"}`, a perfectly ordinary shape — would start receiving a
    /// `400` claiming a conflict it never created. That would be a NEW false statement about what
    /// the request contained, which is the same class of defect #952/#956 is about, just inverted.
    ///
    /// This test is the reason the predicate cannot be "simplified" to `is_some()` later. The
    /// review measured that mutation surviving the entire workspace gate (2008 passed, 0 failed)
    /// because `social.rs` had no tests at all.
    #[tokio::test]
    async fn an_empty_or_blank_add_beside_a_real_remove_is_still_a_removal_not_a_conflict() {
        for blank in ["", "   "] {
            let state = empty_state();
            state.social.friends_list.lock().unwrap().push("Beta".to_string());
            let friends = state.social.friends_list.clone();
            let app = router().with_state(state);
            let resp = app.oneshot(friends_req(
                &format!(r#"{{"add":"{blank}","remove":"Beta"}}"#))).await.unwrap();
            let status = resp.status();
            let text = body_text(resp).await;
            assert_eq!(status, StatusCode::OK,
                "a blank `add` is not a supplied form, so this is an unambiguous removal and must \
                 not be refused as a conflict. add={blank:?} answered {status}: {text}");
            assert_eq!(text, "removed Beta");
            assert!(friends.lock().unwrap().is_empty(),
                "the removal must actually have happened, not merely been reported");
        }
    }

    /// #952/#956: two REAL forms in one body is refused, names both, and changes nothing.
    ///
    /// Before the fix this answered `200 added Alpha` while Beta stayed on the list — the caller was
    /// told its request succeeded and half of it had been discarded.
    #[tokio::test]
    async fn a_real_add_beside_a_real_remove_is_refused_names_both_and_edits_nothing() {
        let state = empty_state();
        state.social.friends_list.lock().unwrap().push("Beta".to_string());
        let friends = state.social.friends_list.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(friends_req(r#"{"add":"Alpha","remove":"Beta"}"#)).await.unwrap();
        let status = resp.status();
        let text = body_text(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "body was: {text}");
        assert!(text.contains("add, remove"),
            "the refusal must name both conflicting fields, in declaration order: {text}");
        assert_eq!(*friends.lock().unwrap(), vec!["Beta".to_string()],
            "a refused request must leave the list exactly as it found it — Alpha must not have \
             been added and Beta must not have been removed");
    }
}
