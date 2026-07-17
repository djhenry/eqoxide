//! `/v1/inventory/*` — inventory management actions (reads live under `/v1/observe/inventory`).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/move", post(post_move))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct MoveBody {
    /// Source slot id (e.g. a general/bag slot like 23, or a worn slot to unequip).
    from: u32,
    /// Destination slot id (e.g. worn slot 19=Feet, 17=Chest; 30=cursor; 22-29 general).
    to: u32,
}

/// POST /v1/inventory/move {"from":N,"to":M} — move/equip/unequip an item between inventory slots.
/// Nav thread sends OP_MoveItem (MoveItem_Struct, number_in_stack=1). Titanium slot ids:
/// 0-21 worn, 22-29 general inventory, 30 cursor, 251+ bag contents.
async fn post_move(
    State(s): State<HttpState>,
    body: Result<Json<MoveBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    if let Err(e) = require_live_session(&s) { return e; }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"from\":N,\"to\":M}".into()),
    };
    s.command.request_inventory_move(b.from, b.to);
    tracing::info!("move: queued from_slot={} to_slot={}", b.from, b.to);
    (StatusCode::OK, format!("moving item from slot {} to slot {}", b.from, b.to))
}
