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
use crate::ipc::TradeCmd;

impl CommandState {
    // ── request_* : the VIEW (UI click-handlers + HTTP handlers) makes these writes ──────────────

    /// Buy merchant inventory slot `slot` from merchant `merchant_id` (POST /v1/merchant/buy, the
    /// merchant window's buy click). The drain opens the merchant then sends OP_ShopPlayerBuy.
    pub fn request_merchant_buy(&self, merchant_id: u32, slot: u32) {
        *self.merchant.buy.lock().unwrap() = Some((merchant_id, slot));
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

    // ── take_* : the MODEL (`ActionLoop::drain_merchant`) drains these once per tick ─────────────

    /// Drain a pending buy request as `(merchant_id, slot)`.
    pub fn take_merchant_buy(&self) -> Option<(u32, u32)> {
        self.merchant.buy.lock().unwrap().take()
    }

    /// Drain a pending sell request as `(merchant_id, slot, quantity)`.
    pub fn take_merchant_sell(&self) -> Option<(u32, u32, u32)> {
        self.merchant.sell.lock().unwrap().take()
    }

    /// Drain a pending open/close request.
    pub fn take_merchant_trade(&self) -> Option<TradeCmd> {
        self.merchant.trade.lock().unwrap().take()
    }
}

#[cfg(test)]
mod tests {
    use super::CommandState;
    use crate::ipc::TradeCmd;

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
    }
}
