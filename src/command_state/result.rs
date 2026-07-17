//! `CommandResult<T>` — the honest outcome of a **Command-with-result** verb (A3 Migration 1, #448).
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! WHY THIS EXISTS  (the agent-honesty gap it closes)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! A plain `request_*` command (see `mod.rs`) is fire-and-forget: the HTTP handler queues the
//! action into an `ipc` slot and immediately returns `200 OK`. But `200` there means "the request
//! was ACCEPTED into the queue", NOT "the action SUCCEEDED". For an action whose real outcome is
//! only knowable from a later server packet — a merchant buy that the server may silently refuse
//! (insufficient funds sends NO packet at all), a trade the merchant may reject, a spell that may
//! fizzle — a premature `200` is the client telling the agent "you bought it" when it does not know
//! whether anything happened. That is a silent wrong answer: the top-priority honesty bug class
//! (see MEMORY `eq-agent-honesty-invariant`).
//!
//! `CommandResult<T>` is the type an HTTP handler AWAITS (over a `oneshot` channel, under a
//! timeout) so it can report the TRUE outcome instead of a queued-action `200`. It mirrors the
//! existing `WhoReq`/`FrameReq` request-reply pattern (HTTP builds a `oneshot`, hands the `Sender`
//! into a `request_*_await` verb, and the net thread fulfils it after the resolving packet is
//! applied) — generalised into a three-way honest outcome.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! THE THREE OUTCOMES  (this is the reference for A3.2 / A3.3 — copy this mapping exactly)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//!   • `Resolved(T)`   → HTTP **200**. A REAL positive server ack landed. `T` carries the honest
//!                       detail read back from the applied receipt (e.g. `BuyOk { item_name,
//!                       price, coin_after }`) — never an optimistic guess made at send time.
//!   • `Refused(String)` → HTTP **409**. A DEFINITIVE negative outcome — either a REAL negative
//!                       server ack (e.g. the merchant's OP_ShopEndConfirm refusal) OR a client-side
//!                       PRE-SEND rejection (e.g. a conflicting awaited command already in flight;
//!                       see the singleton-in-flight discipline below). The `String` is a human
//!                       reason. Distinct from `Unconfirmed`: here we KNOW it is not a success.
//!   • `Unconfirmed`   → HTTP **202**. NO resolving packet arrived within the timeout. The outcome
//!                       is genuinely UNKNOWN — the action may have succeeded, may have been
//!                       silently refused (insufficient funds sends nothing), or the reply may have
//!                       been lost. The body MUST say so and direct the caller to re-check state.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! THE INVARIANT  (do not break this — it is the whole point of A3)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! **`Unconfirmed` MUST NEVER render as success.** It is not `200`, it is not an empty-but-OK
//! body — it is a distinct `202` whose payload explicitly states the outcome is unknown. The HTTP
//! timeout branch (elapsed, channel-closed on a dropped `Sender`, or an explicit `Unconfirmed`
//! from a reaper) all collapse to this same not-success answer. A version of the handler that
//! returned `Resolved`/`200` on timeout is a regression that the honesty tests (silence →
//! `Unconfirmed`) exist to catch.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! THE FLOW  (park → fulfil → time-out), using merchant/buy as the archetype
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! 1. HTTP `post_buy` builds `oneshot::channel::<CommandResult<BuyOk>>()`, calls
//!    `CommandState::request_buy_await(merchant_id, slot, tx)`, and awaits `rx` under a 4 s
//!    `tokio::time::timeout`.
//! 2. `ActionLoop::drain_merchant` takes the await-slot, sends the SAME OP_ShopRequest +
//!    OP_ShopPlayerBuy the fire-and-forget path sends, and PARKS the `Sender` in
//!    `ActionLoop::pending_buy` (with the merchant_id/slot for correlation + a sent-at instant). It
//!    fires NOTHING at send time.
//! 3. In `gameplay.rs`, AFTER `apply_packet` (so `gs` already holds the receipt), the opcode
//!    dispatch fulfils it: the OP_ShopPlayerBuy echo → `fulfill_buy_ok` (a non-blocking
//!    `Sender::send`, correlated on the echo's merchant/slot); OP_ShopEndConfirm →
//!    `fulfill_buy_refused`. The net tick NEVER `.await`s.
//! 4. If no packet correlates within 4 s (the insufficient-funds SILENCE case is the ONLY
//!    resolution for that path), the HTTP timeout elapses → `202` / "outcome UNKNOWN". A
//!    zone-change/disconnect reaper fires `Unconfirmed` for any parked buy so a crossing can't
//!    strand the `Sender` or let it mis-correlate a later shop echo (disconnect is also covered for
//!    free: dropping `ActionLoop` drops the `Sender`, closing the channel → the same 202).
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! SINGLETON-IN-FLIGHT  (a discipline A3.2 / A3.3 MUST copy — do not "improve" it into a queue)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! An awaited command is fulfilled by correlating a later server packet against the parked request.
//! But the server ack carries **NO per-request token** — the OP_ShopPlayerBuy echo, an
//! OP_ShopEndConfirm, etc. do not say WHICH of two identical in-flight requests they answer. So two
//! identical awaited commands in flight at once (e.g. two buys of the same merchant+slot) are
//! **indistinguishable at the echo**: superseding the first with the second would let the first's
//! ack resolve the SECOND caller's `Sender` with the FIRST's receipt — mis-attributing success (a
//! failed second command reporting `Resolved`/200 off the first's success). Perfect correlation is
//! impossible; the only honest fix is to not have two in flight.
//!
//! Therefore awaited commands are **singleton-in-flight**: at most ONE may be parked at a time. When
//! a second awaited command of the same kind arrives while one is parked, the drain does NOT
//! supersede — it REJECTS the new one immediately with `Refused("… already in flight; retry …")`
//! (HTTP 409) and sends NO wire packets for it, so the server only ever processes one at a time and
//! the ack correlation stays unambiguous. The first parked command resolves normally. (409 rather
//! than a 202 `Unconfirmed`, because a pre-send rejection is a thing we KNOW did not happen — the
//! packets were never sent — so "unknown" would understate our certainty.)
//!
//! This is honest and is FINE for naturally-SERIAL verbs — merchant buy (one purchase resolves
//! before the next), a self-cast (one at a time), a give/trade (one trade window at a time). A
//! copier migrating such a verb keeps this discipline. Do NOT replace it with a request queue to
//! allow concurrency: without a per-request token from the server, a queue just relocates the same
//! mis-attribution hazard.
//!
//! KNOWN RESIDUAL (out of reach, documented not fixed): a FIRE-AND-FORGET sibling command (the UI
//! click path, which does not park) of the same kind + slot, issued concurrently with a parked
//! awaited command, could still have its echo resolve the awaited command — because the
//! fire-and-forget path is not gated by `pending_*`. This needs a human click and an agent HTTP
//! call on the exact same slot at the same instant; it is very low likelihood and, critically,
//! cannot fabricate success out of nothing (a real echo did land for a real buy of that slot).
//!
//! NOTE (verified constraint): a `Sender` CANNOT live in `GameState` — it is `Clone`d into the
//! ArcSwap snapshot every tick, and a `oneshot::Sender` is not `Clone`. Park it ONLY in
//! `ActionLoop`.

/// The honest three-way outcome of a command whose success is only knowable from a later server
/// packet. See the module doc for the HTTP status mapping and the never-render-`Unconfirmed`-as-
/// success invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult<T> {
    /// A real positive server ack landed; `T` carries the honest receipt detail. → HTTP 200.
    Resolved(T),
    /// A real negative server ack landed; the `String` is a human-readable reason. → HTTP 409.
    Refused(String),
    /// No resolving packet arrived in time — the outcome is genuinely UNKNOWN. → HTTP 202, with a
    /// body that says so. MUST NOT ever be presented as success.
    Unconfirmed,
}
