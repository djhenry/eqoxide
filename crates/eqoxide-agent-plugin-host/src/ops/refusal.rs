//! The one place in this crate that decides which way a queue outcome maps onto an `OpError`.
//!
//! # Why this module exists (#347, review round 1, finding B1)
//!
//! The first cut of #347 step 2 spelled the refusal out at every call site:
//!
//! `ignore
//! if !s.command.request_pet_command(c) {
//!     return Err(OpError::Conflict(BUSY_PET.into()));
//! }
//! `
//!
//! An independent reviewer deleted the `!` — one character — and the whole suite stayed green
//! (`226 passed; 0 failed`), because both source guards only checked that the `bool` was *read*,
//! never that it was read the right way round. The mutant is #347 restored, with a false refusal
//! on top: with the mailbox **free** it queues the command *and* answers `409 … (it was NOT
//! queued)` (so an agent that trusts "409 is definitive" retries and double-fires), and with the
//! mailbox **busy** it answers `200` for a command that was silently dropped.
//!
//! The fix is not another source guard. It is to remove the thing that was mutated:
//!
//! `ignore
//! if let Some(busy) = s.command.request_pet_command(c).refused(BUSY_PET) { return Err(busy); }
//! `
//!
//! There is now no `!` at any call site to drop, and the polarity decision survives in exactly one
//! place — [`Refusal::refused`] below — where it is covered by this module's own unit tests *and*,
//! transitively, by every per-route 409 test in the crate. Inverting it there is not a silent
//! change: swapping the two match arms of [`Refusal::refused`] was measured at **33 failing tests**
//! in this crate, and the same swap in [`Refusal::refused_status`] at **20** (mutations M-B1d /
//! M-B1e in the PR's mutation table).
//!
//! # What this does and does not achieve
//!
//! This reaches for the "make the bad state unrepresentable rather than guarded" rule from the
//! repo's verification hierarchy. **It does not get there, and three rounds of review have each
//! found a further way to spell the bug**, so the claim is worth stating exactly:
//!
//! - The polarity decision lives at ONE site (this module) rather than at ~38 call sites.
//! - Every silent-drop form found so far is RED, by a source guard and — on eight routes — by a
//!   behavioural double-fire test as well.
//! - The enumeration of those forms is **not** claimed to be complete. It has been wrong four
//!   times: round 1 `if s.command.request_x` (no `!`); round 2 `if let Some(_) = …refused(MSG) { }`,
//!   which compiles and answers `200`; round 2 again, a receiver wrapped across lines, which the
//!   guard's single-line grep never even enumerated; round 3
//!   `if let Some(busy) = … .refused(MSG) { tracing::warn!("{busy:?}"); }`, which binds the refusal
//!   and then answers `200` anyway. Each was found by running a mutation, and each had already been
//!   reasoned away in a comment like this one.
//!
//! Because guessing the next form is what keeps failing, the guard no longer depends on guessing
//! how a call site is *formatted*: it normalises the source (comments stripped, literals blanked,
//! whitespace collapsed, spaces around `.` removed) before matching, and its acceptance predicate
//! is extracted and pinned by a table test over literal snippets — including both wrapped forms.
//! See `guild.rs`'s `no_silent_overwrite_guard`.
//!
//! **Round-3 correction (R3-1).** This paragraph used to end "so relaxing the predicate is itself
//! RED". That was false, and it is the same defect as the ones above, written into the text meant
//! to fix them. The predicate is a three-way conjunction; every negative row in the table failed on
//! conjunct 1 alone, so deleting conjunct 2 or conjunct 3 shipped with the round-2 suite unchanged
//! (`240 passed; 0 failed` either way — the round-3 reviewer's figures, on the round-2 tree), and
//! the composite of the two answered `200` on a refused `interact.sit`. What is true now,
//! measured on THIS tree rather than reasoned (M-R3a / M-R3c / M-R3b, all `239 passed; 1 failed`):
//! the predicate reports WHICH conjuncts a snippet violates, the table pins the exact set per row,
//! and an anti-vacuity assertion requires each conjunct to be the sole fault of some row — so
//! deleting any one of the three is RED. That is a claim about these three conjuncts, not about the
//! enumeration above, which remains open.
//!
//! The part that does not depend on any guard: the four modules the round-1 reviewer measured as
//! having *zero* `409` assertions (`pet`, `trainer`, `group`, `quests`) now each have a double-fire
//! test that fires the same well-formed request twice and asserts `200` then `409` *and* that the
//! FIRST command is the one the net thread drains. Mutations M-B1b / M-B1c / M-B1f / M-R2a.

use crate::ops::wire::OpError;
use eqoxide_agent_protocol::reply::Refused;

/// Maps the `bool` returned by a non-overwriting `CommandState::request_*` onto the refusal
/// response, if any.
///
/// `true` means the message is now parked in a previously-empty mailbox — nothing to answer here,
/// so the handler carries on and reports its own success. `false` means the mailbox already held
/// an undrained message and **nothing was written**, which is the only case that must become a
/// `409`.
///
/// Both methods return `Option<_>` rather than a bare status precisely so the call site has to
/// bind the refusal in order to return it. That binding is what makes the inverted form fail to
/// compile instead of shipping green.
pub(crate) trait Refusal {
    /// `Some(409, msg)` when the command was refused, `None` when it was queued.
    ///
    /// `msg` should say plainly that nothing was queued — the crate's convention is a body ending
    /// `(it was NOT queued)`, so a driving agent can treat the refusal as definitive and retry.
    fn refused(self, msg: &'static str) -> Option<OpError>;

    /// The same decision for the awaited ops (`interact.give`, `merchant.buy`, `merchant.open`)
    /// that answer a refusal with a [`Refused`] body, so the caller can correlate it by
    /// `request_id`.
    fn refused_status(self, reason: &'static str) -> Option<OpError>;
}

impl Refusal for bool {
    fn refused(self, msg: &'static str) -> Option<OpError> {
        match self {
            true => None,
            false => Some(OpError::conflict(msg)),
        }
    }

    fn refused_status(self, reason: &'static str) -> Option<OpError> {
        match self {
            true => None,
            false => Some(OpError::Conflict(Refused::new(reason).into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The polarity itself, stated once. `true` = queued = the handler must go on to report its own
    /// success; `false` = refused = `409`. Every route in the crate inherits this mapping, so this
    /// is the assertion that a swap of the two match arms has to get past first.
    #[test]
    fn queued_is_not_a_refusal_and_refused_is_a_409() {
        assert!(true.refused("busy").is_none(), "a queued command must not produce a refusal");
        let refusal = false.refused("busy").expect("a refused command must produce a 409");
        assert_eq!(refusal, OpError::conflict("busy"));

        assert!(true.refused_status("busy").is_none());
        let refusal = false
            .refused_status("busy")
            .expect("a refused command must produce a 409");
        assert!(matches!(refusal, OpError::Conflict(_)));
    }

    /// The message the caller passes is the message the caller gets — a refusal that reported some
    /// other endpoint's constant would send an agent to the wrong retry.
    #[test]
    fn the_refusal_carries_the_callers_own_message() {
        let refusal = false.refused("a pet command is already queued").unwrap();
        assert_eq!(refusal, OpError::conflict("a pet command is already queued"));
    }

    /// The status refusal carries the `refused` status word and the caller's own reason.
    #[test]
    fn the_status_refusal_body_names_the_reason() {
        let refusal = false.refused_status("busy").unwrap();
        let v = refusal.body();
        assert_eq!(v["status"], "refused");
        assert_eq!(v["reason"], "busy");
        assert!(v.get("request_id").is_none());
    }
}
