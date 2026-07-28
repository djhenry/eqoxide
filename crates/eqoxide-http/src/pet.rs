//! `/v1/pet/*` — manual pet commands (Pet window / agent API). One endpoint: POST `/command`
//! queues an OP_PetCommands command byte into the shared `PetCmdReq` slot; the nav thread drains
//! it and sends the packet (attack aims at the current target). Command values are the EQEmu
//! zone/common.h PET_* constants — see `eqoxide_core::pet`.

use axum::{routing::post, extract::State, Json, http::StatusCode, Router};
use crate::{HttpState, require_live_session};
use crate::refusal::Refusal;

pub fn router() -> Router<HttpState> {
    Router::new().route("/command", post(post_command))
}

/// 409 CONFLICT body for an occupied command slot (#347 step 2). Before #347 a second pet command
/// issued inside the same net-thread tick OVERWROTE the pending one and BOTH callers were told
/// `200`, so one of the two commands silently never happened. A 409 here means the request was NOT
/// queued and definitively did not happen — retrying after the drain is safe.
///
/// Deliberately NOT added here: a "you have a pet" door check. `gs.pet_id` is only ever set from an
/// OP_PetBuffWindow/spawn packet; a missed or late packet would turn a working `/v1/pet/command`
/// into a false 409 — a new lie in the opposite direction. Left as-is pending a measurement of how
/// reliably `pet_id` tracks reality.
const BUSY_PET: &str = "a pet command is already queued and undrained — retry in a moment (it was NOT queued)";

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandBody {
    /// Numeric PET_* command (2=attack, 4=follow, 5=guard here, 6=sit toggle, 28=back off), or
    /// use `name` instead.
    command: Option<u8>,
    /// Friendly alias: "attack" | "backoff" | "follow" | "guard" | "sit".
    name: Option<String>,
}

/// POST /v1/pet/command {"command":N} or {"name":"attack"} — send one pet command. `attack`
/// requires a current target (the nav thread logs and drops it otherwise); the server ignores
/// commands when no pet is up.
async fn post_command(
    State(s): State<HttpState>,
    body: Result<Json<CommandBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    use eqoxide_core::pet::{PET_ATTACK, PET_BACKOFF, PET_FOLLOWME, PET_GUARDHERE, PET_SIT};
    if let Err(e) = require_live_session(&s) { return e; }
    let Ok(Json(b)) = body else {
        return (StatusCode::BAD_REQUEST, "provide {\"command\":N} or {\"name\":\"attack|backoff|follow|guard|sit\"}".into());
    };
    let cmd: Option<u8> = b.command.or_else(|| {
        b.name.as_deref().map(str::to_ascii_lowercase).and_then(|n| match n.as_str() {
            "attack"            => Some(PET_ATTACK as u8),
            "backoff" | "back_off" => Some(PET_BACKOFF as u8),
            "follow"            => Some(PET_FOLLOWME as u8),
            "guard"             => Some(PET_GUARDHERE as u8),
            "sit"               => Some(PET_SIT as u8),
            _ => None,
        })
    });
    match cmd {
        Some(c) => {
            if let Some(busy) = s.command.request_pet_command(c).refused(BUSY_PET) { return busy; }
            (StatusCode::OK, format!("pet command {c} queued"))
        }
        None => (StatusCode::BAD_REQUEST, "unknown pet command — use a PET_* number or attack|backoff|follow|guard|sit".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::empty_state;
    use axum::body::Body;
    use axum::http::Request;
    use eqoxide_core::pet::PET_ATTACK;
    use tower::ServiceExt;

    fn cmd_req(name: &str) -> Request<Body> {
        Request::post("/command")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"name":"{name}"}}"#)))
            .unwrap()
    }

    #[tokio::test]
    async fn a_pet_command_is_200_and_queues_the_command_byte() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(cmd_req("attack")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_pet_command(), Some(PET_ATTACK as u8));
    }

    /// #347 step 2 (review round 1, B1): two pet commands inside one undrained tick. Before the fix
    /// the second OVERWROTE the first and BOTH callers were told `200`, so one command silently
    /// never happened. This route had NO test at all, which is why the reviewer's one-character
    /// mutation at the `request_pet_command` call site shipped fully green — this is the assertion
    /// that mutation now has to get past.
    #[tokio::test]
    async fn a_second_pet_command_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);

        let first = app.clone().oneshot(cmd_req("attack")).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(cmd_req("follow")).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second pet command must be refused, not silently swallowed");

        assert_eq!(command.take_pet_command(), Some(PET_ATTACK as u8),
            "the FIRST command must be the one that survives to the net thread");
        assert_eq!(command.take_pet_command(), None,
            "and nothing else may be queued behind it");
    }
}
