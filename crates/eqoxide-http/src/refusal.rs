//! The one place in this crate that decides which way a queue outcome maps onto a status code.
//!
//! # Why this module exists (#347, review round 1, finding B1)
//!
//! The first cut of #347 step 2 spelled the refusal out at every call site:
//!
//! ```ignore
//! if !s.command.request_pet_command(c) {
//!     return (StatusCode::CONFLICT, BUSY_PET.into());
//! }
//! ```
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
//! ```ignore
//! if let Some(busy) = s.command.request_pet_command(c).refused(BUSY_PET) { return busy; }
//! ```
//!
//! There is now no `!` at any call site to drop, and the polarity decision survives in exactly one
//! place — [`Refusal::refused`] below — where it is covered by this module's own unit tests *and*,
//! transitively, by every per-route 409 test in the crate. Inverting it there is not a silent
//! change: swapping the two match arms of [`Refusal::refused`] was measured at **33 failing tests**
//! in this crate, and the same swap in [`Refusal::refused_json`] at **20** (mutations M-B1d /
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
//! - The enumeration of those forms is **not** claimed to be complete. It has been wrong three
//!   times: round 1 `if s.command.request_x` (no `!`); round 2 `if let Some(_) = …refused(MSG) { }`,
//!   which compiles and answers `200`; round 2 again, a receiver wrapped across lines, which the
//!   guard's single-line grep never even enumerated. Each was found by running a mutation, and each
//!   had already been reasoned away in a comment like this one.
//!
//! Because guessing the next form is what keeps failing, the guard no longer depends on guessing
//! how a call site is *formatted*: it normalises the source (comments stripped, literals blanked,
//! whitespace collapsed, spaces around `.` removed) before matching, and its acceptance predicate
//! is extracted and pinned by a table test over literal snippets — including both wrapped forms —
//! so relaxing the predicate is itself RED. See `guild.rs`'s `no_silent_overwrite_guard`.
//!
//! The part that does not depend on any guard: the four modules the round-1 reviewer measured as
//! having *zero* `409` assertions (`pet`, `trainer`, `group`, `quests`) now each have a double-fire
//! test that fires the same well-formed request twice and asserts `200` then `409` *and* that the
//! FIRST command is the one the net thread drains. Mutations M-B1b / M-B1c / M-B1f / M-R2a.

use axum::{http::StatusCode, response::Response, response::IntoResponse, Json};

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
    fn refused(self, msg: &'static str) -> Option<(StatusCode, String)>;

    /// The same decision for the four handlers that answer JSON (`/interact/give`, `/combat/cast`,
    /// `/merchant/buy`, `/merchant/open`). `body` is the refusal document those endpoints already
    /// documented (`{"status":"refused", …}`); it is built eagerly and dropped when the command was
    /// queued, which costs one small `serde_json::Value` on the success path.
    fn refused_json(self, body: serde_json::Value) -> Option<Response>;
}

impl Refusal for bool {
    fn refused(self, msg: &'static str) -> Option<(StatusCode, String)> {
        match self {
            true => None,
            false => Some((StatusCode::CONFLICT, msg.to_string())),
        }
    }

    fn refused_json(self, body: serde_json::Value) -> Option<Response> {
        match self {
            true => None,
            false => Some((StatusCode::CONFLICT, Json(body)).into_response()),
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
        let (status, body) = false.refused("busy").expect("a refused command must produce a 409");
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body, "busy");

        assert!(true.refused_json(serde_json::json!({})).is_none());
        let resp = false
            .refused_json(serde_json::json!({ "status": "refused" }))
            .expect("a refused command must produce a 409");
        assert_eq!(resp.status(), StatusCode::CONFLICT);
    }

    /// The message the caller passes is the message the caller gets — a refusal that reported some
    /// other endpoint's constant would send an agent to the wrong retry.
    #[test]
    fn the_refusal_carries_the_callers_own_message() {
        let (_, body) = false.refused("a pet command is already queued").unwrap();
        assert_eq!(body, "a pet command is already queued");
    }

    /// The JSON refusal body is passed through untouched, so an endpoint that documents extra
    /// fields (`/combat/cast` carries `"landed": false`) keeps them.
    #[tokio::test]
    async fn the_json_refusal_body_is_passed_through_verbatim() {
        let resp = false
            .refused_json(serde_json::json!({ "status": "refused", "landed": false, "reason": "busy" }))
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["status"], "refused");
        assert_eq!(v["landed"], false);
        assert_eq!(v["reason"], "busy");
    }
}
