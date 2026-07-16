//! `/v1/merchant/*` — vendor interaction: open/close a merchant window, list wares, buy, sell.

use axum::{extract::State, http::StatusCode, routing::{get, post}, Json, Router};
use super::*;

pub(super) fn router() -> Router<HttpState> {
    Router::new()
        .route("/open", post(post_trade_open))
        .route("/close", post(post_trade_close))
        .route("/list", get(get_trade_list))
        .route("/buy", post(post_buy))
        .route("/sell", post(post_sell))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct TradeOpenBody {
    /// Merchant NPC name (fuzzy-matched, like /merchant/buy).
    merchant: String,
}

/// POST /v1/merchant/open {"merchant":"<name>"} — open the named merchant's window (OP_ShopRequest).
/// Must be within ~200u. The server replies Open (window opens, items arrive) or Close (refused,
/// e.g. KOS faction); watch GET /v1/merchant/list `open` to see the result.
async fn post_trade_open(
    State(s): State<HttpState>,
    body: Result<Json<TradeOpenBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\"}".into()),
    };
    let ids = s.world.entity_ids.lock().unwrap();
    let nl = b.merchant.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            s.command.request_merchant_trade(TradeCmd::Open(id));
            tracing::info!("trade: queued open merchant {:?} (spawn_id={})", key, id);
            (StatusCode::OK, format!("opening merchant {} (spawn_id={})", clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
}

/// POST /v1/merchant/close — close the currently open merchant window (OP_ShopRequest command=Close).
async fn post_trade_close(State(s): State<HttpState>) -> (StatusCode, String) {
    s.command.request_merchant_trade(TradeCmd::Close);
    (StatusCode::OK, "closing merchant window".into())
}

/// GET /v1/merchant/list — the open merchant's offered items (for buying). Returns `{open,
/// merchant_id, count, items:[{merchant_slot,item_id,name,icon,price,quantity}]}`. `open:false`
/// means no merchant window is open (never opened, was closed, or the merchant refused, e.g. KOS).
async fn get_trade_list(State(s): State<HttpState>) -> Json<serde_json::Value> {
    let m = s.merchant_slots.merchant.lock().unwrap();
    Json(serde_json::json!({
        "open": m.open,
        "merchant_id": m.merchant_id,
        "count": m.items.len(),
        "items": m.items,
    }))
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct BuyBody {
    /// Merchant NPC name (fuzzy-matched, like /combat/target/name).
    merchant: String,
    /// Merchant inventory slot of the item to buy (from /v1/merchant/list).
    slot: u32,
}

/// POST /v1/merchant/buy {"merchant":"<name>","slot":N} — open the named merchant and buy item slot
/// N. Must be within ~200u of the merchant. The nav thread sends OP_ShopRequest then OP_ShopPlayerBuy.
async fn post_buy(
    State(s): State<HttpState>,
    body: Result<Json<BuyBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\",\"slot\":N}".into()),
    };
    let ids = s.world.entity_ids.lock().unwrap();
    let nl = b.merchant.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            s.command.request_merchant_buy(id, b.slot);
            tracing::info!("buy: queued merchant {:?} (spawn_id={}) slot={}", key, id, b.slot);
            (StatusCode::OK, format!("buying slot {} from {} (spawn_id={})", b.slot, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SellBody {
    /// Merchant NPC name (fuzzy-matched, like /merchant/buy).
    merchant: String,
    /// Player inventory slot of the item to sell — the RoF2 wire slot `/v1/observe/inventory`
    /// reports (general inventory 23-32, bag contents 251+).
    slot: u32,
    /// Number to sell from the slot (stack count). Defaults to 1.
    quantity: Option<u32>,
}

/// POST /v1/merchant/sell {"merchant":"<name>","slot":N,"quantity":Q} — open the named merchant and
/// sell the item in player inventory slot N (quantity Q, default 1). Must be within ~200u. The nav
/// thread sends OP_ShopRequest then OP_ShopPlayerSell (price computed server-side).
async fn post_sell(
    State(s): State<HttpState>,
    body: Result<Json<SellBody>, axum::extract::rejection::JsonRejection>,
) -> (StatusCode, String) {
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return (StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\",\"slot\":N,\"quantity\":Q}".into()),
    };
    let qty = b.quantity.unwrap_or(1).max(1);
    let ids = s.world.entity_ids.lock().unwrap();
    let nl = b.merchant.to_lowercase();
    let found = ids.iter()
        .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
        .map(|(k, &id)| (k.clone(), id));
    match found {
        Some((key, id)) => {
            s.command.request_merchant_sell(id, b.slot, qty);
            tracing::info!("sell: queued merchant {:?} (spawn_id={}) slot={} qty={}", key, id, b.slot, qty);
            (StatusCode::OK, format!("selling slot {} x{} to {} (spawn_id={})", b.slot, qty, clean_entity_name(&key), id))
        }
        None => (StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    }
}

#[cfg(test)]
mod tests {
    use super::router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use crate::http::quests::tests::empty_state;

    fn seed_merchant(state: &crate::http::HttpState, key: &str, id: u32) {
        state.world.entity_ids.lock().unwrap().insert(key.to_string(), id);
    }

    /// eqoxide#341: a typo'd key ("quantitiy" instead of "quantity") must 400 — not be silently
    /// ignored by serde (leaving `quantity` at its default `None`, which `post_sell` then treats as
    /// "caller omitted it" and defaults to selling quantity=1).
    #[tokio::test]
    async fn sell_unknown_key_is_400_and_does_not_queue() {
        let state = empty_state();
        seed_merchant(&state, "Innkeeper_Beek000", 11);
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/sell")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"merchant":"Beek","slot":23,"quantitiy":5}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(command.take_merchant_sell().is_none(),
            "a typo'd key must not silently fall through to selling with quantity defaulted to 1");
    }

    #[tokio::test]
    async fn sell_valid_body_still_queues() {
        let state = empty_state();
        seed_merchant(&state, "Innkeeper_Beek000", 11);
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/sell")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"merchant":"Beek","slot":23,"quantity":5}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(command.take_merchant_sell(), Some((11, 23, 5)));
    }
}
