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
    let (tx, rx) = oneshot::channel::<Vec<crate::game_state::WhoEntry>>();
    *s.social.friends_req.lock().unwrap() = Some(tx);
    match tokio::time::timeout(std::time::Duration::from_secs(6), rx).await {
        Ok(Ok(online_roster)) => {
            // Index the online subset by lowercased name for annotation.
            let online: std::collections::HashMap<String, crate::game_state::WhoEntry> =
                online_roster.into_iter().map(|e| (e.name.to_lowercase(), e)).collect();
            let list: Vec<FriendView> = friends.into_iter().map(|name| {
                match online.get(&name.to_lowercase()) {
                    Some(e) => FriendView {
                        name, online: true,
                        zone_id: Some(e.zone_id),
                        level:   if e.anon { None } else { Some(e.level) },
                        class:   if e.anon { None } else { Some(crate::eq_net::packet_handler::class_name(e.class).to_string()) },
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
