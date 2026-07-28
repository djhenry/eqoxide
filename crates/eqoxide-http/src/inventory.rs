//! `/v1/inventory/*` — inventory management actions (reads live under `/v1/observe/inventory`).

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/move", post(post_move))
}

/// 409 CONFLICT body for an occupied command slot (#347 step 2). Before #347 a second move issued
/// inside the same net-thread tick OVERWROTE the pending one and BOTH callers were told `200`, so
/// one of the two moves silently never happened. A 409 here means the request was NOT queued and
/// definitively did not happen — retrying after the drain is safe.
const BUSY_MOVE: &str = "an inventory move is already queued and undrained — retry in a moment (it was NOT queued)";

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
    // #347 step 1 (reject at the door): an empty `from` makes the whole move a no-op — the drain
    // sends OP_MoveItem for a slot the server knows is empty and `GameState::move_item` returns
    // early — yet the caller was told 200 "moving item". Checked against the last published
    // inventory (GET /v1/observe/inventory), the same source `/v1/interact/read` validates against.
    let occupied = s.inventory_slots.inventory.lock().unwrap().iter().any(|i| i.slot == b.from as i32);
    if !occupied {
        return (StatusCode::NOT_FOUND, format!("no item in slot {} to move", b.from));
    }
    if !s.command.request_inventory_move(b.from, b.to) {
        return (StatusCode::CONFLICT, BUSY_MOVE.into());
    }
    tracing::info!("move: queued from_slot={} to_slot={}", b.from, b.to);
    (StatusCode::OK, format!("moving item from slot {} to slot {}", b.from, b.to))
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::testkit::empty_state;

    /// Publish one item at a wire slot, exactly as `OP_CharInventory` decoding does, so the door
    /// check under test sees the same state a live client would.
    fn seed_item(state: &crate::HttpState, slot: i32) {
        state.inventory_slots.inventory.lock().unwrap().push(eqoxide_core::game_state::InvItem {
            slot, item_id: 13073, name: "Bone Chips".into(), charges: 1, ..Default::default()
        });
    }

    fn move_req(from: u32, to: u32) -> Request<Body> {
        Request::post("/move")
            .header("content-type", "application/json")
            .body(Body::from(format!(r#"{{"from":{from},"to":{to}}}"#))).unwrap()
    }

    /// #347 step 1: moving FROM a slot the published inventory says is empty is a no-op the server
    /// silently discards — it must be refused at the door, not answered `200 moving item`.
    #[tokio::test]
    async fn move_from_an_empty_slot_is_404_and_queues_nothing() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(move_req(23, 30)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert_eq!(command.take_inventory_move(), None,
            "a rejected move must not reach the command slot at all");
    }

    #[tokio::test]
    async fn move_from_an_occupied_slot_is_200_and_queues_it() {
        let state = empty_state();
        seed_item(&state, 23);
        let command = state.command.clone();
        let app = router().with_state(state);
        let resp = app.oneshot(move_req(23, 30)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_inventory_move(), Some((23, 30)));
    }

    /// #347 step 2, the honesty core: two moves inside one undrained tick. Before the fix the second
    /// OVERWROTE the first and BOTH callers were told `200` — one move silently never happened.
    /// Now the second is refused and the FIRST is what the net thread drains.
    #[tokio::test]
    async fn a_second_move_before_the_drain_is_409_and_the_first_survives() {
        let state = empty_state();
        seed_item(&state, 23);
        seed_item(&state, 24);
        let command = state.command.clone();
        let app = router().with_state(state);

        let first = app.clone().oneshot(move_req(23, 30)).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);

        let second = app.oneshot(move_req(24, 31)).await.unwrap();
        assert_eq!(second.status(), StatusCode::CONFLICT,
            "the second move must be refused, not silently swallowed");

        assert_eq!(command.take_inventory_move(), Some((23, 30)),
            "the FIRST move must be the one that survives to the net thread");
        assert_eq!(command.take_inventory_move(), None,
            "and nothing else may be queued behind it");
    }
}
