//! `CommandResult<T>` — the honest outcome of a **Command-with-result** verb (A3 Migration 1, #448).
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! WHY THIS EXISTS  (the agent-honesty gap it closes)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! A plain `request_*` command (see the app crate's `command_state` module) is fire-and-forget: the op
//! handler queues the action into an `ipc` slot and immediately returns `200 OK`. But `200` there
//! means "the request was ACCEPTED into the queue", NOT "the action SUCCEEDED". For an action whose
//! real outcome is only knowable from a later server packet — a merchant buy that the server may
//! silently refuse (insufficient funds sends NO packet at all), a trade the merchant may reject, a
//! spell that may fizzle — a premature `200` is the client telling the agent "you bought it" when it
//! does not know whether anything happened. That is a silent wrong answer: the top-priority honesty
//! bug class (see MEMORY `eq-agent-honesty-invariant`).
//!
//! `CommandResult<T>` is the type an agent op AWAITS (over a `oneshot` channel, under a
//! timeout) so it can report the TRUE outcome instead of a queued-action `200`. It mirrors the
//! existing `WhoReq` request-reply pattern (the op builds a `oneshot`, hands the `Sender`
//! into a `request_*_await` verb, and the net thread fulfils it after the resolving packet is
//! applied) — generalised into a three-way honest outcome.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! THE THREE OUTCOMES  (this is the reference for A3.2 / A3.3 — copy this mapping exactly)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//!   • `Resolved(T)`   → status **200**. A REAL positive server ack landed. `T` carries the honest
//!                       detail read back from the applied receipt (e.g. `BuyOk { item_name,
//!                       price, coin_after }`) — never an optimistic guess made at send time.
//!   • `Refused(String)` → status **409**. A DEFINITIVE negative outcome — either a REAL negative
//!                       server ack (e.g. the merchant's OP_ShopEndConfirm refusal) OR a client-side
//!                       PRE-SEND rejection (e.g. a conflicting awaited command already in flight;
//!                       see the singleton-in-flight discipline below). The `String` is a human
//!                       reason. Distinct from `Unconfirmed`: here we KNOW it is not a success.
//!   • `Unconfirmed`   → status **202**. NO resolving packet arrived within the timeout. The outcome
//!                       is genuinely UNKNOWN — the action may have succeeded, may have been
//!                       silently refused (insufficient funds sends nothing), or the reply may have
//!                       been lost. The body MUST say so and direct the caller to re-check state.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! THE INVARIANT  (do not break this — it is the whole point of A3)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! **`Unconfirmed` MUST NEVER render as success.** It is not `200`, it is not an empty-but-OK
//! body — it is a distinct `202` whose payload explicitly states the outcome is unknown. The ops
//! timeout branch (elapsed, channel-closed on a dropped `Sender`, or an explicit `Unconfirmed`
//! from a reaper) all collapse to this same not-success answer. A version of the handler that
//! returned `Resolved`/`200` on timeout is a regression that the honesty tests (silence →
//! `Unconfirmed`) exist to catch.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! THE FLOW  (park → fulfil → time-out), using merchant/buy as the archetype
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! 1. the op `post_buy` builds `oneshot::channel::<CommandResult<BuyOk>>()`, calls
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
//!    resolution for that path), the op timeout elapses → `202` / "outcome UNKNOWN". A
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
//! (status 409) and sends NO wire packets for it, so the server only ever processes one at a time and
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
//! fire-and-forget path is not gated by `pending_*`. This needs a human click and an agent ops
//! call on the exact same slot at the same instant; it is very low likelihood and, critically,
//! cannot fabricate success out of nothing (a real echo did land for a real buy of that slot).
//!
//! NOTE (verified constraint): a `Sender` CANNOT live in `GameState` — it is `Clone`d into the
//! ArcSwap snapshot every tick, and a `oneshot::Sender` is not `Clone`. Park it ONLY in
//! `ActionLoop`.
//!
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! LOCATION  (#557 — Step 1b of the #544 modularity hybrid path)
//! ────────────────────────────────────────────────────────────────────────────────────────────
//! `CommandResult<T>` and its payload types (`BuyOk`, `OpenOk`, `GiveOk`, `CastEnd`) live HERE, in
//! `ipc`, not in `command_state` — because `ipc`'s own await-slot types (`BuyAwaitReq`,
//! `OpenAwaitReq`, `GiveAwaitReq`, `CastAwaitReq`) hold `oneshot::Sender<CommandResult<T>>` fields
//! and so MUST reference these types. `command_state` depends on `ipc` (the slot bundles), so
//! having these result types live in `command_state` while `ipc` referenced them back would be an
//! illegal dependency cycle once the two split into separate crates (`eqoxide-ipc`,
//! `eqoxide-command`). `command_state` re-exports these types (`pub use eqoxide_ipc::{...}`) so every
//! existing `crate::command_state::CommandResult`/`BuyOk`/`OpenOk`/`GiveOk`/`CastEnd` call site is
//! unaffected — this was a pure code-motion, not a rename.

/// The honest three-way outcome of a command whose success is only knowable from a later server
/// packet. See the module doc for the status code mapping and the never-render-`Unconfirmed`-as-
/// success invariant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResult<T> {
    /// A real positive server ack landed; `T` carries the honest receipt detail. → status 200.
    Resolved(T),
    /// A real negative server ack landed; the `String` is a human-readable reason. → status 409.
    Refused(String),
    /// No resolving packet arrived in time — the outcome is genuinely UNKNOWN. → status 202, with a
    /// body that says so. MUST NOT ever be presented as success.
    Unconfirmed,
}

/// The honest receipt of a confirmed merchant buy (A3 Migration 1, #448) — the `T` in
/// `CommandResult<BuyOk>` and the JSON body of a 200 from POST /v1/merchant/buy. Every field is
/// read back from the APPLIED OP_ShopPlayerBuy echo (`gs` after `apply_packet`), never guessed at
/// send time: `price` is the server-recomputed price from the echo, `coin_after` is the balance
/// AFTER the server's deduction was mirrored locally. See this module's doc.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BuyOk {
    /// The purchased item's name, resolved from the open merchant's ware list by the echoed slot.
    pub item_name: String,
    /// Price the server actually charged (from the echo — the server recomputes it).
    pub price: u32,
    /// Coin on hand (platinum, gold, silver, copper) AFTER the buy was applied locally.
    pub coin_after: [u32; 4],
}

/// The honest receipt of a confirmed merchant open (eqoxide#479) — the `T` in
/// `CommandResult<OpenOk>` and the JSON body of a 200 from POST /v1/merchant/open. `merchant_id` is
/// read back from the APPLIED OP_ShopRequest echo (`command==1`), never guessed at send time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct OpenOk {
    /// The spawn id of the merchant that confirmed the open (echoed npc_id).
    pub merchant_id: u32,
}

/// The honest receipt of a confirmed NPC turn-in (A3 Migration 2, #448) — the `T` in
/// `CommandResult<GiveOk>` and the JSON body of a 200 from POST /v1/interact/give. It records WHAT
/// was handed in (`item_name`, captured from the inventory slot at send time — the trade slots are
/// already cleared by the time the confirming OP_FinishTrade is applied, so it cannot be read back
/// then) and to WHOM (`npc_id`). Unlike a merchant buy there is no server-recomputed receipt to
/// mirror: OP_FinishTrade is a 0-byte "the NPC accepted it" ack, so `GiveOk` is the honest statement
/// "this item's turn-in to this NPC was ACCEPTED" — only ever sent on a real OP_FinishTrade, never a
/// send-time guess. See this module's doc.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GiveOk {
    /// Spawn id of the NPC the item was handed to (the give's target).
    pub npc_id: u32,
    /// Name of the item that was turned in, captured from the inventory slot at send time.
    pub item_name: String,
}

/// The honest terminal outcome of an awaited self-cast (A3 Migration 3, #448) — the `T` in
/// `CommandResult<CastEnd>` and the JSON body of a 200 from POST /v1/combat/cast. Read back from the
/// APPLIED cast machinery's `gs.last_cast`, never guessed at send time.
///
/// A `Resolved(CastEnd)` means the server gave a DEFINITE verdict on the cast — but "definite" is not
/// the same as "the spell landed". `outcome` carries that truth: only `"completed"` is a success;
/// `"fizzled"` and `"interrupted"` are resolved NON-successes. This is the whole honesty point of
/// carrying the outcome in a field rather than in the status code: a 200 can never be misread as
/// "the spell took hold" — an agent MUST branch on `outcome`. A cast whose outcome is unknown
/// (silent server, timeout, zone change) is `Unconfirmed`/202, never a `CastEnd`; a cast that never
/// started (empty gem, no mana, no target) is `Refused`/409. See this module's doc.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CastEnd {
    /// The honest terminal verdict: `"completed"` (the spell landed) | `"fizzled"` | `"interrupted"`.
    /// NEVER `"completed"` unless the server actually reported completion.
    pub outcome: String,
    /// The spell id that ended, or 0 when the server never named it (an honest unknown, not a guess).
    pub spell_id: u32,
    /// The spell's name (resolved from `spell_id`; a placeholder when the id is 0/unknown).
    pub spell_name: String,
    /// The human-readable line the cast machinery recorded (also in the message log).
    pub text: String,
}
