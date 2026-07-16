//! `/v1/guild/*` — guild membership: roster (who's in the guild + online status) and the
//! join/leave/invite/remove actions. Mirrors `/v1/group/*`. Guild identity (name/id/rank) is also
//! surfaced on `/v1/observe/debug`. (#295)

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/roster", get(get_roster))
        .route("/invite", post(post_invite))
        .route("/accept", post(post_accept))
        .route("/leave", post(post_leave))
        .route("/remove", post(post_remove))
}

/// GET /v1/guild/roster — the player's guild identity and full member roster. `members` empty (and
/// `guild_id` 0) means not in a guild. Each member carries online status + last-seen zone so an
/// agent can route guild messages to who's actually present.
async fn get_roster(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let g = s.guild_slots.guild.lock().unwrap();
    let members: Vec<serde_json::Value> = g.members.iter().map(|m| serde_json::json!({
        "name":    m.name,
        "rank":    m.rank,
        "level":   m.level,
        "class":   m.class,
        "zone_id": m.zone_id,
        "online":  m.online,
        "public_note": m.public_note,
    })).collect();
    Json(serde_json::json!({
        "guild":          g.guild_name,
        "guild_id":       g.guild_id,
        "guild_rank":     g.guild_rank,
        "pending_invite": g.pending_invite,
        "members":        members,
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

/// Queue a single guild action (rejecting if one is already pending and undrained).
fn queue(s: &HttpState, action: GuildAction) -> (StatusCode, String) {
    let mut slot = s.guild_slots.guild_action.lock().unwrap();
    if slot.is_some() {
        return (StatusCode::CONFLICT, "a guild action is already pending".into());
    }
    let msg = match &action {
        GuildAction::Invite(n) => format!("inviting {n} to the guild"),
        GuildAction::Accept    => "accepting guild invite".into(),
        GuildAction::Leave     => "leaving guild".into(),
        GuildAction::Remove(n) => format!("removing {n} from the guild"),
    };
    *slot = Some(action);
    (StatusCode::OK, msg)
}

/// POST /v1/guild/invite {"name":"X"} — invite player X to our guild (requires invite rights).
async fn post_invite(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    queue(&s, GuildAction::Invite(name))
}

/// POST /v1/guild/accept — accept a pending guild invite. 400 if none is pending.
async fn post_accept(State(s): State<HttpState>) -> (StatusCode, String) {
    if s.guild_slots.guild.lock().unwrap().pending_invite.is_none() {
        return (StatusCode::BAD_REQUEST, "no pending guild invite".into());
    }
    queue(&s, GuildAction::Accept)
}

/// POST /v1/guild/leave — leave the current guild.
async fn post_leave(State(s): State<HttpState>) -> (StatusCode, String) {
    if s.guild_slots.guild.lock().unwrap().guild_id == 0 {
        return (StatusCode::BAD_REQUEST, "not in a guild".into());
    }
    queue(&s, GuildAction::Leave)
}

/// POST /v1/guild/remove {"name":"X"} — remove member X (guild leader / GM only, server-enforced).
async fn post_remove(
    State(s): State<HttpState>,
    body: Result<Json<NameBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let name = match extract_name(body) { Ok(n) => n, Err(e) => return e };
    queue(&s, GuildAction::Remove(name))
}
