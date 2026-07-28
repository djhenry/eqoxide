//! [`Mailbox`] — the ONE way a `request_*` writes a single-slot command mailbox (#347 step 2).
//!
//! Every view→model command in this crate lives in an `Arc<Mutex<Option<T>>>` that `ActionLoop`
//! drains once per tick. Before #347 every `request_*` wrote it with a blind
//! `*slot.lock().unwrap() = Some(x)`, so a second request arriving inside the drain window
//! **replaced** the first — and the HTTP handler that made the first request had already answered
//! `200`. One of the two actions simply never happened and nothing said so. That is the structural
//! form of the agent-honesty defect: the client reported a confident falsehood.
//!
//! [`Mailbox::try_put`] is the fix: it is a check-then-set under ONE lock acquisition, and it
//! **never overwrites**. An occupied mailbox is left exactly as it was and the caller gets `false`,
//! which the HTTP handlers turn into `409 CONFLICT`. `guild.rs`'s `request_guild_action` did this
//! by hand before #347 and was the only slot in the codebase that did; this trait generalizes it.
//!
//! ## Why a trait rather than a guard call
//! `try_put`/[`Mailbox::take_msg`] are the only slot operations this crate's `request_*`/`take_*`
//! methods use, so "overwrite a pending command" is not something a domain module can express by
//! accident — it has to be written out longhand as a raw `*slot.lock().unwrap() = ..`. The
//! [`tests::no_domain_module_blind_writes_a_command_slot`] source guard below then fails on any
//! such longhand write that is not explicitly annotated `LAST-WINS`, so the rule cannot be
//! reintroduced silently by a later domain migration. (Making it *statically* unrepresentable would
//! mean changing the slot's declared type in `eqoxide-ipc`, which every drain site and several
//! other crates name; that is a bigger blast radius than #347's scope allows.)
//!
//! ## What is deliberately NOT a `Mailbox`
//! Three domains keep last-writer-wins semantics, each annotated `LAST-WINS` at the write site:
//!   * **nav** (`nav.rs`) — `/move/goto`, `/move/follow`, `/move/stop`, `/move/zone_cross` are a
//!     RETARGET, not a queue: a second goto is meant to supersede the first. Crucially, nav is also
//!     the one domain that is already honest about it — every write stamps a fresh `goal_id` and
//!     publishes `nav_state`/`nav_reason` (#349/#725), so a superseded goal is observable to the
//!     agent. It is not in the silent-drop class this module addresses.
//!   * **lifecycle** (`lifecycle.rs`) — `camp` is a toggle whose LAST value is the intended one, and
//!     `POST /v1/lifecycle/exit` must be able to override an in-progress camp (it is the only way to
//!     tear down a wedged session). `respawn` is a plain `bool` flag, not an `Option`: setting it
//!     twice loses nothing.
//!   * **social** (`social.rs`) — `who_req`/`friends_req` hold a `oneshot::Sender`, not a command.
//!     Dropping one does not produce a false `200`: the losing caller's receiver closes and its
//!     handler answers `503`. Out of #347's "both callers got 200" class, so left unchanged.
//!
//! `chat.rs`'s `chat_send` is a `Vec`, i.e. already an unbounded FIFO, and was the only loss-free
//! slot before this change.

use std::sync::Mutex;

/// A single-slot command mailbox that refuses to lose a message.
///
/// Implemented for `Mutex<Option<T>>`, which is what every command slot in `eqoxide_ipc` is behind
/// an `Arc`. See the module docs for why this exists.
pub(crate) trait Mailbox<T> {
    /// Put `msg` in the mailbox if it is empty. Returns `true` if it was accepted.
    ///
    /// Returns `false` — leaving the pending message **untouched** — if a message is already
    /// waiting to be drained. Check-then-set happens under a single lock acquisition, so two
    /// concurrent `try_put`s can never both succeed.
    fn try_put(&self, msg: T) -> bool;

    /// Drain the mailbox, leaving it empty. The MODEL side (`ActionLoop::tick`) calls this.
    fn take_msg(&self) -> Option<T>;

    /// `true` if a message is waiting to be drained — a PEEK that leaves the mailbox alone.
    ///
    /// Exists so a test can observe "the first request is still parked" without `take_msg`ing it,
    /// which would destroy the very occupancy the next assertion depends on.
    fn is_occupied(&self) -> bool;
}

impl<T> Mailbox<T> for Mutex<Option<T>> {
    fn try_put(&self, msg: T) -> bool {
        let mut slot = self.lock().unwrap();
        if slot.is_some() {
            return false;
        }
        *slot = Some(msg);
        true
    }

    fn take_msg(&self) -> Option<T> {
        self.lock().unwrap().take()
    }

    fn is_occupied(&self) -> bool {
        self.lock().unwrap().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::Mailbox;
    use std::sync::Mutex;

    #[test]
    fn try_put_fills_an_empty_mailbox() {
        let m: Mutex<Option<u32>> = Mutex::new(None);
        assert!(m.try_put(7));
        assert_eq!(m.take_msg(), Some(7));
        assert_eq!(m.take_msg(), None, "a drained mailbox must not re-fire");
    }

    #[test]
    fn try_put_on_an_occupied_mailbox_is_refused_and_preserves_the_first_message() {
        let m: Mutex<Option<u32>> = Mutex::new(None);
        assert!(m.try_put(1));
        assert!(!m.try_put(2), "the second message must be refused");
        assert_eq!(m.take_msg(), Some(1), "the FIRST message must survive untouched");
    }

    /// Under real concurrency, exactly one of N racing `try_put`s wins and the winner's message is
    /// the one that drains — no interleaving can produce two accepted puts or a lost winner.
    #[test]
    fn concurrent_try_puts_accept_exactly_one() {
        use std::sync::{atomic::{AtomicUsize, Ordering}, Arc, Barrier};

        for _round in 0..200 {
            let m: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
            let accepted = Arc::new(AtomicUsize::new(0));
            let barrier = Arc::new(Barrier::new(4));
            let handles: Vec<_> = (0..4)
                .map(|i| {
                    let (m, accepted, barrier) = (m.clone(), accepted.clone(), barrier.clone());
                    std::thread::spawn(move || {
                        barrier.wait();
                        if m.try_put(i) {
                            accepted.fetch_add(1, Ordering::SeqCst);
                            Some(i)
                        } else {
                            None
                        }
                    })
                })
                .collect();
            let winners: Vec<usize> = handles.into_iter().filter_map(|h| h.join().unwrap()).collect();
            assert_eq!(accepted.load(Ordering::SeqCst), 1, "exactly one put may be accepted");
            assert_eq!(m.take_msg(), Some(winners[0]), "the accepted put's message must be the one that drains");
        }
    }

    /// Source guard (#347): no domain module in this crate may write a command slot with a blind
    /// `*self.<domain>.<slot>.lock().unwrap() = Some(..)` again. Every such longhand write must
    /// carry an explicit `LAST-WINS` annotation on the write line or in the comment block
    /// immediately above it — the documented escape hatch for the three deliberately
    /// last-writer-wins domains (see the module docs).
    ///
    /// This is a static rule, so it is checked statically: the runtime tests below can only prove
    /// the methods that exist TODAY reject an occupied slot, whereas #347's claim is universal over
    /// this crate's write path. A future domain migration that reintroduces the blind write fails
    /// here even if nobody writes a behavior test for it.
    ///
    /// SCOPE, stated exactly: this looks for the blind PUT (`= Some(..)`) only. A blind CLEAR
    /// (`= None`) can also destroy a queued command, but the only `= None` writes in this crate are
    /// nav's deliberate goto/zone-cross cancels, so flagging that form would be pure annotation
    /// churn and is not claimed to be covered.
    ///
    /// **Why this one stayed line-based when `eqoxide-http`'s guard did not (round 2, N2 / round
    /// 3).** That guard was rewritten to normalise the source because a call site reflowed across
    /// lines silently left its coverage — the guard was the ONLY thing standing behind those call
    /// sites, so a formatting change could delete a check. That argument does not transfer here,
    /// and the reason is stronger than "the risk is lower": this crate's real universal is
    /// [`every_no_overwrite_request_refuses_a_second_write`], which is BEHAVIOURAL — it issues a
    /// second write to a live slot and asserts the first message is the one that drains — and its
    /// case table is completeness-checked against [`declared_no_overwrite_requests`], itself read
    /// out of the sources. **No reformatting evades a behavioural test.** Wrap a write across five
    /// lines and the second write still has to be refused, or that test fails. This static rule is
    /// a redundant early-warning on top of it, catching a *newly added* blind write before anyone
    /// writes a behaviour test for it.
    #[test]
    fn no_domain_module_blind_writes_a_command_slot() {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut scanned = 0usize;
        let mut offenders: Vec<String> = Vec::new();
        let mut annotated = 0usize;

        for entry in std::fs::read_dir(&src).expect("crate src/ must be readable") {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            let text = std::fs::read_to_string(&path).expect("readable source file");
            scanned += 1;
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                // Only the WRITE form, and only a real assignment statement — `*self.<a>.<b>…`.
                // Anchoring on `*self.` (rather than a bare substring) is what keeps this test from
                // matching its own source, or `//!`/`///` prose that quotes the pattern.
                let code = line.trim_start();
                if !(code.starts_with("*self.") && code.contains(".lock().unwrap() = Some(")) {
                    continue;
                }
                // The marker may sit on the write line or anywhere in the contiguous comment block
                // directly above it, so the REASON can be written out at length next to the write.
                let mut marked = code.contains("LAST-WINS");
                let mut k = n;
                while !marked && k > 0 && lines[k - 1].trim_start().starts_with("//") {
                    k -= 1;
                    marked = lines[k].contains("LAST-WINS");
                }
                if marked {
                    annotated += 1;
                } else {
                    offenders.push(format!("{name}:{}: {}", n + 1, code));
                }
            }
        }

        assert!(scanned >= 10, "expected to scan the crate's domain modules, scanned only {scanned}");
        assert!(
            offenders.is_empty(),
            "blind command-slot write(s) found — use `Mailbox::try_put` (which refuses rather than \
             overwrites), or annotate the line `LAST-WINS` with the reason if the domain genuinely \
             wants last-writer-wins (see slot.rs module docs, #347):\n{}",
            offenders.join("\n"),
        );
        // The guard is only meaningful if it is actually looking at lines. If the annotated count
        // ever drops to zero the pattern has drifted and the assert above became vacuous.
        assert!(
            annotated > 0,
            "the guard matched no write lines at all — the search pattern has drifted from the source",
        );
    }

    /// Every `request_*` in this crate that returns `bool` — i.e. every no-overwrite command slot —
    /// paired with the name it is covered under in
    /// [`every_no_overwrite_request_refuses_a_second_write`]. Read out of the crate's OWN sources
    /// rather than hand-listed, so a new domain verb that forgets a no-overwrite test fails the
    /// completeness assertion instead of quietly shipping untested.
    ///
    /// Search key, if you need to re-run this by hand:
    /// `grep -rn 'pub fn request_' crates/eqoxide-command/src/*.rs` — then keep the ones whose
    /// signature (which may span several lines, up to the opening `{`) contains `-> bool`.
    fn declared_no_overwrite_requests() -> Vec<String> {
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&src).expect("crate src/ must be readable") {
            let path = entry.expect("readable dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("readable source file");
            let lines: Vec<&str> = text.lines().collect();
            for (n, line) in lines.iter().enumerate() {
                let code = line.trim_start();
                let Some(rest) = code.strip_prefix("pub fn request_") else { continue };
                let name = format!("request_{}", rest.split(['(', '<']).next().unwrap_or(""));
                // Accumulate the signature up to the opening brace (multi-line `request_*_await`s).
                let mut sig = String::new();
                for l in lines.iter().skip(n) {
                    sig.push_str(l);
                    if l.trim_end().ends_with('{') {
                        break;
                    }
                }
                if sig.contains("-> bool") {
                    found.push(name);
                }
            }
        }
        found.sort();
        found
    }

    /// **#347 step 2, as a universal over the facade.** For EVERY no-overwrite command slot: a
    /// second `request_*` issued while one is still pending is REFUSED (`false`), and the message
    /// that drains is the FIRST one — never the second, never nothing. This is what makes the
    /// HTTP-side `409` truthful; before #347 the second write clobbered the first and BOTH callers
    /// had already been told `200`.
    ///
    /// The case table is checked for completeness against [`declared_no_overwrite_requests`], so
    /// "every slot" is an enumeration this test verifies rather than one a human asserted.
    #[test]
    fn every_no_overwrite_request_refuses_a_second_write() {
        use crate::CommandState;
        use eqoxide_core::game_state::DialogueChoice;
        use eqoxide_ipc::{CastRequest, GuildAction, TradeCmd};

        let mut covered: Vec<String> = Vec::new();

        /// first/second write + drain, asserting the FIRST survives.
        macro_rules! case {
            ($name:literal, $first:expr, $second:expr, $drain:expr, $expected:expr $(,)?) => {{
                covered.push($name.to_string());
                let cs = CommandState::default();
                let first: fn(&CommandState) -> bool = $first;
                let second: fn(&CommandState) -> bool = $second;
                assert!(first(&cs), "{}: the first request must be accepted", $name);
                assert!(!second(&cs), "{}: a second request over a pending one must be REFUSED", $name);
                let drain: fn(&CommandState) -> _ = $drain;
                assert_eq!(drain(&cs), $expected, "{}: the FIRST request must be the one that drains", $name);
            }};
        }

        case!("request_target", |c| c.request_target(11), |c| c.request_target(22), |c| c.take_target(), Some(11));
        case!("request_attack", |c| c.request_attack(true), |c| c.request_attack(false), |c| c.take_attack(), Some(true));
        case!("request_consider", |c| c.request_consider(11), |c| c.request_consider(22), |c| c.take_consider(), Some(11));
        case!("request_cast",
            |c| c.request_cast(CastRequest { gem: 1, target_id: None, item_slot: None }),
            |c| c.request_cast(CastRequest { gem: 2, target_id: None, item_slot: None }),
            |c| c.take_cast().map(|r| r.gem), Some(1));
        case!("request_mem_spell",
            |c| c.request_mem_spell(0, 100, 1, None), |c| c.request_mem_spell(1, 200, 1, None),
            |c| c.take_mem_spell(), Some((0, 100, 1, None)));
        case!("request_pet_command", |c| c.request_pet_command(2), |c| c.request_pet_command(4), |c| c.take_pet_command(), Some(2));

        case!("request_merchant_buy", |c| c.request_merchant_buy(11, 3), |c| c.request_merchant_buy(11, 4),
            |c| c.take_merchant_buy(), Some((11, 3)));
        case!("request_merchant_sell", |c| c.request_merchant_sell(11, 23, 1), |c| c.request_merchant_sell(11, 24, 1),
            |c| c.take_merchant_sell(), Some((11, 23, 1)));
        case!("request_merchant_trade", |c| c.request_merchant_trade(TradeCmd::Open(11)), |c| c.request_merchant_trade(TradeCmd::Close),
            |c| matches!(c.take_merchant_trade(), Some(TradeCmd::Open(11))), true);

        case!("request_inventory_move", |c| c.request_inventory_move(23, 30), |c| c.request_inventory_move(24, 30),
            |c| c.take_inventory_move(), Some((23, 30)));

        case!("request_hail", |c| c.request_hail("A".into(), Some(1)), |c| c.request_hail("B".into(), Some(2)),
            |c| c.take_hail(), Some(("A".to_string(), Some(1))));
        case!("request_say", |c| c.request_say("first".into()), |c| c.request_say("second".into()),
            |c| c.take_say(), Some("first".to_string()));
        case!("request_loot", |c| c.request_loot(9), |c| c.request_loot(10), |c| c.take_loot(), Some(9));
        case!("request_give", |c| c.request_give(11, 23), |c| c.request_give(11, 24), |c| c.take_give(), Some((11, 23)));
        case!("request_door_click", |c| c.request_door_click(3), |c| c.request_door_click(4), |c| c.take_door_click(), Some(3));
        case!("request_sit", |c| c.request_sit(true), |c| c.request_sit(false), |c| c.take_sit(), Some(true));
        case!("request_run_mode", |c| c.request_run_mode(true), |c| c.request_run_mode(false), |c| c.take_run_mode(), Some(true));
        case!("request_dialogue_click",
            |c| c.request_dialogue_click(DialogueChoice { text: "first".into(), ..Default::default() }),
            |c| c.request_dialogue_click(DialogueChoice { text: "second".into(), ..Default::default() }),
            |c| c.take_dialogue_click().map(|d| d.text), Some("first".to_string()));
        case!("request_read_book", |c| c.request_read_book(23), |c| c.request_read_book(24), |c| c.take_read_book(), Some(23));

        case!("request_accept_task", |c| c.request_accept_task(42), |c| c.request_accept_task(43), |c| c.take_accept_task(), Some(42));
        case!("request_cancel_task", |c| c.request_cancel_task(42), |c| c.request_cancel_task(43), |c| c.take_cancel_task(), Some(42));

        case!("request_group_invite", |c| c.request_group_invite("A".into()), |c| c.request_group_invite("B".into()),
            |c| c.take_group_invite(), Some("A".to_string()));
        case!("request_group_accept", |c| c.request_group_accept(), |c| c.request_group_accept(), |c| c.take_group_accept(), Some(()));
        case!("request_group_decline", |c| c.request_group_decline(), |c| c.request_group_decline(), |c| c.take_group_decline(), Some(()));
        case!("request_group_leave", |c| c.request_group_leave(), |c| c.request_group_leave(), |c| c.take_group_leave(), Some(()));
        case!("request_group_kick", |c| c.request_group_kick("A".into()), |c| c.request_group_kick("B".into()),
            |c| c.take_group_kick(), Some("A".to_string()));
        case!("request_group_make_leader", |c| c.request_group_make_leader("A".into()), |c| c.request_group_make_leader("B".into()),
            |c| c.take_group_make_leader(), Some("A".to_string()));

        case!("request_open_trainer", |c| c.request_open_trainer(11), |c| c.request_open_trainer(22),
            |c| c.take_trainer_open(), Some(11));
        case!("request_train_skill", |c| c.request_train_skill(5), |c| c.request_train_skill(6),
            |c| c.take_train_skill(), Some(5));

        case!("request_guild_action",
            |c| c.request_guild_action(GuildAction::Invite("A".into())),
            |c| c.request_guild_action(GuildAction::Leave),
            |c| c.take_guild_action(), Some(GuildAction::Invite("A".into())));

        // The four awaited slots carry a `oneshot::Sender`, which is neither `Debug` nor `PartialEq`,
        // so they can't ride the `case!` table. Same three assertions, written out: first accepted,
        // second refused, and the drained sender is the FIRST caller's — proven by SENDING on it and
        // observing it at the first caller's receiver. That last step matters: an overwrite would
        // drop `tx_a`, so `rx_a` would close and the first caller's `/give` (or `/cast`, `/buy`,
        // `/open`) would answer 202 "unconfirmed" for an action that in fact never went out.
        macro_rules! await_case {
            ($name:literal, $ok:ty, $resolved:expr, $req:expr, $take:expr $(,)?) => {{
                covered.push($name.to_string());
                let cs = CommandState::default();
                let (tx_a, mut rx_a) = tokio::sync::oneshot::channel::<crate::CommandResult<$ok>>();
                let (tx_b, mut rx_b) = tokio::sync::oneshot::channel::<crate::CommandResult<$ok>>();
                let req: fn(&CommandState, tokio::sync::oneshot::Sender<crate::CommandResult<$ok>>) -> bool = $req;
                assert!(req(&cs, tx_a), "{}: the first request must be accepted", $name);
                assert!(!req(&cs, tx_b), "{}: a second request over a pending one must be REFUSED", $name);
                let take: fn(&CommandState) -> Option<tokio::sync::oneshot::Sender<crate::CommandResult<$ok>>> = $take;
                let drained = take(&cs).unwrap_or_else(|| panic!("{}: the first request must still be drainable", $name));
                drained.send(crate::CommandResult::Resolved($resolved)).ok();
                assert!(rx_a.try_recv().is_ok(), "{}: the drained sender must be the FIRST caller's", $name);
                assert!(rx_b.try_recv().is_err(), "{}: the second caller must not have been fulfilled", $name);
            }};
        }

        await_case!("request_cast_await", crate::CastEnd,
            crate::CastEnd { outcome: "completed".into(), spell_id: 1, spell_name: "S".into(), text: String::new() },
            |c, tx| c.request_cast_await(CastRequest { gem: 1, target_id: None, item_slot: None }, tx),
            |c| c.take_cast_await().map(|(_, tx)| tx));
        await_case!("request_buy_await", crate::BuyOk,
            crate::BuyOk { item_name: "I".into(), price: 1, coin_after: [0; 4] },
            |c, tx| c.request_buy_await(11, 3, tx),
            |c| c.take_buy_await().map(|(_, _, tx)| tx));
        await_case!("request_open_await", crate::OpenOk,
            crate::OpenOk { merchant_id: 11 },
            |c, tx| c.request_open_await(11, tx),
            |c| c.take_open_await().map(|(_, tx)| tx));
        await_case!("request_give_await", crate::GiveOk,
            crate::GiveOk { npc_id: 11, item_name: "I".into() },
            |c, tx| c.request_give_await(11, 23, tx),
            |c| c.take_give_await().map(|(_, _, tx)| tx));

        covered.sort();
        let declared = declared_no_overwrite_requests();
        assert_eq!(
            covered, declared,
            "the no-overwrite case table and the crate's `-> bool` `request_*` methods have diverged \
             — every no-overwrite slot must be exercised here",
        );
        assert!(declared.len() >= 30, "expected the facade's full no-overwrite surface, found {}", declared.len());
    }
}
