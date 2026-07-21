//! Merchant command verbs — Wave-2 migration of the `combat.rs` pattern (see `mod.rs` "HOW TO
//! MIGRATE A DOMAIN").
//!
//! Domain: `/v1/merchant/*` (open/close/buy/sell) plus the matching HUD merchant window. Every
//! method is a thin typed read/write of a slot in `self.merchant`; validation and packet-building
//! stay where they were (the HTTP handler / UI window and `ActionLoop::drain_merchant`). No
//! behavior change — just one typed surface. `buy`/`sell`/`trade` are prefixed `merchant_` because
//! "buy"/"sell"/"trade" are generic commerce verbs another domain could plausibly reuse.
//!
//! `self.merchant.merchant` (the live `MerchantSnapshot` for GET /v1/merchant/list) is a
//! read-path/published field, not a command — deliberately NOT exposed here (see `mod.rs`).

use super::CommandState;
use eqoxide_ipc::{BuyOk, CommandResult, OpenOk, TradeCmd};
use tokio::sync::oneshot;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Buy merchant inventory slot `slot` from merchant `merchant_id` (the merchant window's buy
    /// click — FIRE-AND-FORGET). The drain opens the merchant then sends OP_ShopPlayerBuy. HTTP's
    /// POST /v1/merchant/buy uses the awaited [`request_buy_await`](Self::request_buy_await) instead.
    pub fn request_merchant_buy(&self, merchant_id: u32, slot: u32) {
        *self.merchant.buy.lock().unwrap() = Some((merchant_id, slot));
    }

    /// Command-with-result buy (A3 Migration 1, #448): queue the SAME buy as `request_merchant_buy`
    /// but hand in a `oneshot::Sender<CommandResult<BuyOk>>` the net thread fulfils with the TRUE
    /// outcome. POST /v1/merchant/buy awaits the matching receiver under a timeout so it reports the
    /// real result instead of a premature queued-action 200. Writes the sibling `buy_await` slot —
    /// the fire-and-forget `buy` slot (UI path) is left untouched. See `crate::command_state::result`.
    pub fn request_buy_await(
        &self,
        merchant_id: u32,
        slot: u32,
        tx: oneshot::Sender<CommandResult<BuyOk>>,
    ) {
        *self.merchant.buy_await.lock().unwrap() = Some((merchant_id, slot, tx));
    }

    /// Sell `quantity` of player inventory slot `slot` to merchant `merchant_id` (POST
    /// /v1/merchant/sell, the merchant window's sell click).
    pub fn request_merchant_sell(&self, merchant_id: u32, slot: u32, quantity: u32) {
        *self.merchant.sell.lock().unwrap() = Some((merchant_id, slot, quantity));
    }

    /// Open or close the merchant window (POST /v1/merchant/{open,close}, the merchant window's
    /// close button, and the transient-window-close handler for `registry::MERCHANT`).
    pub fn request_merchant_trade(&self, cmd: TradeCmd) {
        *self.merchant.trade.lock().unwrap() = Some(cmd);
    }

    /// Command-with-result open (eqoxide#479): queue the SAME open as
    /// `request_merchant_trade(TradeCmd::Open(..))` but hand in a `oneshot::Sender<CommandResult<OpenOk>>`
    /// the net thread fulfils with the TRUE outcome. POST /v1/merchant/open awaits the matching
    /// receiver under a timeout so it reports the real result instead of a premature queued-action
    /// 200. Writes the sibling `open_await` slot — the fire-and-forget `trade` slot (UI path) is left
    /// untouched. See `crate::command_state::result`.
    pub fn request_open_await(
        &self,
        merchant_id: u32,
        tx: oneshot::Sender<CommandResult<OpenOk>>,
    ) {
        *self.merchant.open_await.lock().unwrap() = Some((merchant_id, tx));
    }

    // ── take_* : the MODEL (`ActionLoop::drain_merchant`) drains these once per tick ─────────────

    /// Drain a pending buy request as `(merchant_id, slot)`.
    pub fn take_merchant_buy(&self) -> Option<(u32, u32)> {
        self.merchant.buy.lock().unwrap().take()
    }

    /// Drain a pending awaited-buy request as `(merchant_id, slot, Sender)` (A3 Migration 1, #448).
    /// `ActionLoop::drain_merchant` takes this, sends the buy, and parks the `Sender` in
    /// `pending_buy` until the resolving packet lands. Sibling of `take_merchant_buy`.
    pub fn take_buy_await(
        &self,
    ) -> Option<(u32, u32, oneshot::Sender<CommandResult<BuyOk>>)> {
        self.merchant.buy_await.lock().unwrap().take()
    }

    /// Drain a pending sell request as `(merchant_id, slot, quantity)`.
    pub fn take_merchant_sell(&self) -> Option<(u32, u32, u32)> {
        self.merchant.sell.lock().unwrap().take()
    }

    /// Drain a pending open/close request.
    pub fn take_merchant_trade(&self) -> Option<TradeCmd> {
        self.merchant.trade.lock().unwrap().take()
    }

    /// Drain a pending awaited-open request as `(merchant_id, Sender)` (eqoxide#479).
    /// `ActionLoop::drain_merchant` takes this, sends the open, and parks the `Sender` in
    /// `pending_open` until the resolving packet lands. Sibling of `take_merchant_trade`.
    pub fn take_open_await(&self) -> Option<(u32, oneshot::Sender<CommandResult<OpenOk>>)> {
        self.merchant.open_await.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use eqoxide_ipc::TradeCmd;

    /// The core invariant of the facade: a `request_*` write and the matching `take_*` drain touch
    /// the SAME slot, the drain removes it, and a second drain sees nothing. Proven for every
    /// merchant verb.
    #[test]
    fn request_then_take_round_trips_each_merchant_slot() {
        let cs = CommandState::default();

        cs.request_merchant_buy(11, 3);
        assert_eq!(cs.take_merchant_buy(), Some((11, 3)));
        assert_eq!(cs.take_merchant_buy(), None, "a drained buy must not re-fire");

        cs.request_merchant_sell(11, 23, 5);
        assert_eq!(cs.take_merchant_sell(), Some((11, 23, 5)));
        assert_eq!(cs.take_merchant_sell(), None);

        cs.request_merchant_trade(TradeCmd::Open(11));
        assert!(matches!(cs.take_merchant_trade(), Some(TradeCmd::Open(11))));
        assert!(cs.take_merchant_trade().is_none());

        cs.request_merchant_trade(TradeCmd::Close);
        assert!(matches!(cs.take_merchant_trade(), Some(TradeCmd::Close)));
        assert!(cs.take_merchant_trade().is_none());
    }

    /// A slot with nothing queued drains to `None` — no phantom command.
    #[test]
    fn take_on_empty_slot_is_none() {
        let cs = CommandState::default();
        assert_eq!(cs.take_merchant_buy(), None);
        assert_eq!(cs.take_merchant_sell(), None);
        assert!(cs.take_merchant_trade().is_none());
        assert!(cs.take_buy_await().is_none());
        assert!(cs.take_open_await().is_none());
    }

    /// eqoxide#479: the awaited-open slot round-trips the `(merchant_id, Sender)` tuple, the drain
    /// removes it, and the drained `Sender` still reaches its receiver — proving `open_await` is a
    /// genuine sibling of the fire-and-forget `trade` slot and does NOT disturb it.
    #[tokio::test]
    async fn request_open_await_round_trips_the_sender_and_leaves_trade_untouched() {
        use super::{OpenOk, CommandResult};
        let cs = CommandState::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<CommandResult<OpenOk>>();

        cs.request_open_await(11, tx);
        // The fire-and-forget UI slot is a genuinely separate cell — an awaited open must not queue it.
        assert!(cs.take_merchant_trade().is_none(),
            "the awaited open must not leak into the fire-and-forget UI slot");

        let (mid, drained_tx) = cs.take_open_await().expect("awaited open must drain");
        assert_eq!(mid, 11);
        assert!(cs.take_open_await().is_none(), "a drained awaited open must not re-fire");

        drained_tx.send(CommandResult::Resolved(OpenOk { merchant_id: 11 })).unwrap();
        assert_eq!(rx.await.unwrap(), CommandResult::Resolved(OpenOk { merchant_id: 11 }));
    }

    /// A3 Migration 1 (#448): the awaited-buy slot round-trips the `(merchant_id, slot, Sender)`
    /// tuple, the drain removes it, and the drained `Sender` still reaches its receiver — proving
    /// `buy_await` is a genuine sibling of the fire-and-forget `buy` slot and does NOT disturb it.
    #[tokio::test]
    async fn request_buy_await_round_trips_the_sender_and_leaves_buy_untouched() {
        use super::{BuyOk, CommandResult};
        let cs = CommandState::default();
        let (tx, rx) = tokio::sync::oneshot::channel::<CommandResult<BuyOk>>();

        cs.request_buy_await(11, 3, tx);
        // The fire-and-forget UI slot is a genuinely separate cell — an awaited buy must not queue it.
        assert_eq!(cs.take_merchant_buy(), None,
            "the awaited buy must not leak into the fire-and-forget UI slot");

        let (mid, slot, drained_tx) = cs.take_buy_await().expect("awaited buy must drain");
        assert_eq!((mid, slot), (11, 3));
        assert!(cs.take_buy_await().is_none(), "a drained awaited buy must not re-fire");

        drained_tx.send(CommandResult::Resolved(BuyOk {
            item_name: "Rusty Dagger".into(), price: 5, coin_after: [0, 0, 0, 95],
        })).unwrap();
        assert_eq!(
            rx.await.unwrap(),
            CommandResult::Resolved(BuyOk { item_name: "Rusty Dagger".into(), price: 5, coin_after: [0, 0, 0, 95] }),
        );
    }
}
