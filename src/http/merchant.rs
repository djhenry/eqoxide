//! `/v1/merchant/*` — vendor interaction: open/close a merchant window, list wares, buy, sell.

use axum::{
    body::Body,
    extract::State,
    http::{header, StatusCode},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use tokio::sync::oneshot;
use std::time::Duration;
use crate::command_state::{BuyOk, CommandResult};
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
    if let Err(e) = require_live_session(&s) { return e; }
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
    if let Err(e) = require_live_session(&s) { return e; }
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

/// A quick `(StatusCode, String)` plain-text response, so the small error paths stay terse.
fn text(status: StatusCode, body: impl Into<String>) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from(body.into())).unwrap()
}

/// A JSON response with an explicit status.
fn json(status: StatusCode, value: serde_json::Value) -> Response {
    Response::builder().status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(value.to_string())).unwrap()
}

/// POST /v1/merchant/buy {"merchant":"<name>","slot":N} — open the named merchant and buy item slot
/// N. Must be within ~200u of the merchant. The nav thread sends OP_ShopRequest then OP_ShopPlayerBuy.
///
/// A3 Migration 1 (#448) — Command-with-result: this no longer returns a premature "queued" 200. It
/// AWAITS the real outcome (up to 4s) and reports it honestly:
///   • 200 — the server CONFIRMED the buy (OP_ShopPlayerBuy echo). Body: `{status:"bought", item,
///     price, coin_after}` read back from the applied receipt.
///   • 409 — the server REFUSED it (OP_ShopEndConfirm). Body: `{status:"refused", reason}`.
///   • 202 — the outcome is UNKNOWN: no resolving packet arrived within 4s. This is what an
///     INSUFFICIENT-FUNDS buy produces, because the server sends NOTHING at all on that path (it
///     also covers a lost reply or a zone change mid-buy). The body says so explicitly and points at
///     the state to re-check. A 202 MUST NOT be read as success — that is the whole honesty
///     invariant of A3 (see `crate::command_state::result`).
async fn post_buy(
    State(s): State<HttpState>,
    body: Result<Json<BuyBody>, axum::extract::rejection::JsonRejection>,
) -> Response {
    if let Err((code, msg)) = require_live_session(&s) { return text(code, msg); }
    let b = match body {
        Ok(Json(b)) => b,
        Err(_) => return text(StatusCode::BAD_REQUEST, "provide {\"merchant\":\"...\",\"slot\":N}"),
    };
    // Resolve the merchant, then DROP the entity map lock before awaiting — never hold a std Mutex
    // across an `.await`.
    let found = {
        let ids = s.world.entity_ids.lock().unwrap();
        let nl = b.merchant.to_lowercase();
        ids.iter()
            .find(|(k, _)| clean_entity_name(k).to_lowercase().contains(&nl) || k.to_lowercase().contains(&nl))
            .map(|(k, &id)| (k.clone(), id))
    };
    let (key, id) = match found {
        Some(hit) => hit,
        None => return text(StatusCode::NOT_FOUND, format!("no merchant matching {:?}", b.merchant)),
    };

    // Park the buy with a result channel and await the TRUE outcome (park → fulfil → timeout).
    let (tx, rx) = oneshot::channel::<CommandResult<BuyOk>>();
    s.command.request_buy_await(id, b.slot, tx);
    tracing::info!("buy: awaited buy queued — merchant {:?} (spawn_id={}) slot={}", key, id, b.slot);

    match tokio::time::timeout(Duration::from_secs(4), rx).await {
        // A REAL confirmation echo landed — honest success with the applied receipt detail.
        Ok(Ok(CommandResult::Resolved(BuyOk { item_name, price, coin_after }))) => json(
            StatusCode::OK,
            serde_json::json!({
                "status": "bought",
                "item": item_name,
                "price": price,
                "coin_after": currency_json(coin_after),
            }),
        ),
        // A REAL refusal echo landed (OP_ShopEndConfirm).
        Ok(Ok(CommandResult::Refused(reason))) => json(
            StatusCode::CONFLICT,
            serde_json::json!({ "status": "refused", "reason": reason }),
        ),
        // Unconfirmed, channel closed (Sender dropped — disconnect/superseded), or elapsed: the
        // outcome is genuinely UNKNOWN. Insufficient funds is exactly this case (the server is
        // silent). MUST NOT read as success — 202 with an explicit "re-check state" body.
        _ => json(
            StatusCode::ACCEPTED,
            serde_json::json!({
                "status": "unconfirmed",
                "message": "buy sent to merchant, but the outcome is UNKNOWN — no confirmation \
                            arrived within 4s. It may have failed silently (e.g. insufficient funds, \
                            which the server does not acknowledge) or the reply was lost. Re-check \
                            GET /v1/observe/inventory for the item and the coin_verified flag before \
                            assuming it succeeded.",
                "slot": b.slot,
                "merchant": clean_entity_name(&key),
            }),
        ),
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
    if let Err(e) = require_live_session(&s) { return e; }
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

    // ── A3 Migration 1 (#448): POST /v1/merchant/buy reports the TRUE outcome, not a queued 200 ──

    use crate::command_state::{BuyOk, CommandResult};

    /// A buy that resolves nowhere-to-target still 404s before parking anything.
    #[tokio::test]
    async fn buy_unknown_merchant_is_404() {
        let state = empty_state();
        let command = state.command.clone();
        let app = router().with_state(state);
        let req = Request::post("/buy").header("content-type", "application/json")
            .body(Body::from(r#"{"merchant":"Nobody","slot":3}"#)).unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(command.take_buy_await().is_none(), "a 404 must not park a buy");
    }

    /// SUCCESS: the server confirms the buy (OP_ShopPlayerBuy echo, delivered here as `Resolved`) →
    /// 200 with the honest receipt body (item/price/coin_after).
    #[tokio::test]
    async fn buy_confirmed_is_200_with_the_receipt() {
        let state = empty_state();
        seed_merchant(&state, "Innkeeper_Beek000", 11);
        let command = state.command.clone();
        let app = router().with_state(state);
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/buy").header("content-type", "application/json")
                .body(Body::from(r#"{"merchant":"Beek","slot":3}"#)).unwrap()).await.unwrap()
        });
        // Wait for the handler to park its Sender, then deliver the confirmed receipt.
        let (mid, slot, tx) = loop {
            if let Some(p) = command.take_buy_await() { break p; }
            tokio::task::yield_now().await;
        };
        assert_eq!((mid, slot), (11, 3));
        tx.send(CommandResult::Resolved(BuyOk {
            item_name: "Rusty Dagger".into(), price: 5, coin_after: [1, 2, 3, 4],
        })).unwrap();

        let resp = task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "bought");
        assert_eq!(v["item"], "Rusty Dagger");
        assert_eq!(v["price"], 5);
        assert_eq!(v["coin_after"]["platinum"], 1);
    }

    /// REFUSAL: a real negative ack (OP_ShopEndConfirm → `Refused`) → 409.
    #[tokio::test]
    async fn buy_refused_is_409() {
        let state = empty_state();
        seed_merchant(&state, "Innkeeper_Beek000", 11);
        let command = state.command.clone();
        let app = router().with_state(state);
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/buy").header("content-type", "application/json")
                .body(Body::from(r#"{"merchant":"Beek","slot":3}"#)).unwrap()).await.unwrap()
        });
        let (_m, _s, tx) = loop {
            if let Some(p) = command.take_buy_await() { break p; }
            tokio::task::yield_now().await;
        };
        tx.send(CommandResult::Refused("merchant refused".into())).unwrap();

        let resp = task.await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "refused");
    }

    /// INSUFFICIENT-FUNDS SILENCE — THE HONESTY PROOF at the HTTP boundary. The server sends NOTHING,
    /// so nothing ever fires the parked Sender; the 4s timeout elapses (virtual time under
    /// `start_paused`, so the test is instant) → **202**, NOT 200. The body must say the outcome is
    /// UNKNOWN. The Sender is HELD (not dropped) across the wait, so this exercises the genuine
    /// ELAPSED branch, not a channel-closed shortcut.
    #[tokio::test(start_paused = true)]
    async fn buy_with_no_server_reply_is_202_unknown_never_success() {
        let state = empty_state();
        seed_merchant(&state, "Innkeeper_Beek000", 11);
        let command = state.command.clone();
        let app = router().with_state(state);
        let task = tokio::spawn(async move {
            app.oneshot(Request::post("/buy").header("content-type", "application/json")
                .body(Body::from(r#"{"merchant":"Beek","slot":3}"#)).unwrap()).await.unwrap()
        });
        // Take the parked Sender and HOLD it — the server's silence, faithfully modelled.
        let held = loop {
            if let Some(p) = command.take_buy_await() { break p; }
            tokio::task::yield_now().await;
        };

        let resp = task.await.unwrap(); // 4s timeout elapses in virtual time
        assert_ne!(resp.status(), StatusCode::OK,
            "a silent (e.g. insufficient-funds) buy MUST NOT be reported as success — the A3 invariant");
        assert_eq!(resp.status(), StatusCode::ACCEPTED);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["status"], "unconfirmed");
        let msg = v["message"].as_str().unwrap();
        assert!(msg.contains("UNKNOWN"), "the body must state the outcome is unknown");
        assert!(msg.contains("insufficient funds"), "and point at the likely silent-failure cause");
        drop(held);
    }
}
