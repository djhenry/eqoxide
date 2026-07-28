//! `/v1/pet/*` — manual pet commands (Pet window / agent API). One endpoint: POST `/command`
//! queues an OP_PetCommands command byte into the shared `PetCmdReq` slot; the nav thread drains
//! it and sends the packet (attack aims at the current target). Command values are the EQEmu
//! zone/common.h PET_* constants — see `eqoxide_core::pet`.

use axum::{routing::post, extract::State, Json, http::StatusCode, Router};
use crate::{HttpState, require_live_session};

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
            if !s.command.request_pet_command(c) {
                return (StatusCode::CONFLICT, BUSY_PET.into());
            }
            (StatusCode::OK, format!("pet command {c} queued"))
        }
        None => (StatusCode::BAD_REQUEST, "unknown pet command — use a PET_* number or attack|backoff|follow|guard|sit".into()),
    }
}
