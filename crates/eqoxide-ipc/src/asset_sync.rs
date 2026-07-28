//! The agent-observable **asset-sync activity** registry (#715).
//!
//! ## Why this exists
//!
//! Zone/common asset sync progress (phase, chunks done/total, bytes, download rate) was rendered
//! only on the loading-screen HUD. This client is driven by an AI agent that has no eyes on the
//! screen: during a zone load it could observe *that* the client was not ready (via
//! [`crate::…`]-adjacent `zone_assets`, #579) but nothing about whether the load was progressing,
//! stalled, nearly done, or wedged. This is that missing channel, served on
//! `GET /v1/observe/asset_sync`.
//!
//! ## Why a REGISTRY and not a single slot
//!
//! This started life as one `Option<AssetSyncActivity>` cell with last-writer-wins, cleared by each
//! guard's `Drop` when the published `set` still matched its own. The #726 review measured why that
//! cannot hold: the client runs **three concurrent loaders**, and the model-sync worker's short
//! `charmodel/<key>` sync routinely begins *and ends* **inside** the zone loader's long
//! `zone/<zone>` download. The nested guard took the slot on `begin`, so on `Drop` the set check
//! matched its own value and it cleared — blanking a zone download that was still in flight, and
//! answering `{"active": false}` while it ran.
//!
//! That is not a flicker. If the outer sync is wedged **mid-chunk** nothing ever republishes, so
//! the endpoint reads idle for the whole wedge; if it is wedged in [`AssetSyncPhase::Starting`] (a
//! hung manifest request) it has published exactly once, at `begin`, and will never publish again.
//! The wedge this endpoint exists to expose would become invisible, and the endpoint would serve
//! the healthiest possible answer while it happened.
//!
//! So each sync **owns its own entry**, keyed by an opaque monotonic [`SyncId`] rather than by its
//! set name. A guard can only ever remove the entry it created. Keying by `set` would have fixed
//! the measured case but not the general one: two syncs of the *same* set can overlap (a re-zone
//! into the zone already loading, the same `charmodel/<key>` requested twice), and there the second
//! `begin` would evict the first and the first `Drop` would delete the second — the identical
//! defect one step down. An id makes ownership exact rather than probable.
//!
//! ## Why an enum, and why the transfer data is nested
//!
//! Two distinctions this type exists to keep, both of which a flat struct would destroy:
//!
//! 1. **Phase is modelled, not flattened.** The producer's `asset_sync::SyncProgress` is an enum
//!    precisely so a download rate cannot be read outside the downloading phase (#708). Mirroring
//!    it as a flat struct with nullable `bytes`/`rate` fields would throw that away at the API
//!    boundary — a reader seeing `rate: null` could not tell "not downloading" from "downloading,
//!    rate not yet derivable". [`AssetSyncPhase::Downloading`] is the ONLY variant that carries
//!    transfer data, so a stale rate is structurally unrepresentable here just as it is upstream.
//!
//! 2. **"No sync in progress" is an EMPTY registry, not a zeroed struct.** An idle client and a
//!    download stalled at 0/N are different situations an agent acts on differently.
//!
//! ## Why a LOGIN is a different kind of entry, not a phase (#731)
//!
//! Every `sync_set` call is preceded by an asset-server `login()` — an HTTP POST to `/auth` that
//! can be slow, can hang, and (until #731) sat entirely outside the observed window. The endpoint
//! answered `{"active": false}` — "no asset sync is running" — while a loader thread was blocked
//! inside it. That is the same falsehood the registry above exists to prevent, one step earlier in
//! the call.
//!
//! #731 suggested an `AssetSyncPhase::Connecting` variant with one guard spanning login *and* sync.
//! That shape does not survive contact with the call sites: it needs a `set` name, and **three of
//! the four logins in this client do not have one**. The model-sync worker logs in once and then
//! serves an unbounded queue of `charmodel/<key>` sets; startup logs in once for `gamedata` *and*
//! `gameequip`; the zone loader's single login covers both `zone/<z>` and `zonedoors/<z>`. Only
//! `common` is 1:1. A `set` field filled in at those sites would be a guess, and a guess dressed as
//! an answer is the defect this endpoint exists to avoid.
//!
//! So a login is modelled as its own [`AssetSyncWork`] variant. A login **has no set, no chunks, no
//! bytes and no rate**, and in this shape it cannot acquire them: [`AssetSyncWork::Connecting`]
//! carries only a free-text `purpose`, and only [`AssetSyncWork::Sync`] has a
//! [`AssetSyncPhase`] at all. A login can therefore never masquerade as a transfer stalled at 0
//! bytes — which would have been a subtler version of the same lie.
//!
//! What it DOES inherit, for free and coherently, is the staleness machinery: a login is one atomic
//! request that publishes exactly once, at `begin`, and never ticks — structurally identical to a
//! sync wedged in [`AssetSyncPhase::Starting`]. Its `published_age_ms` therefore grows for as long
//! as it is blocked and feeds `stalest_published_age_ms` like any other entry, so the documented
//! one-field wedge check reports a hung login without any special case.
//!
//! ## Who writes it, and when an entry is REMOVED
//!
//! The sole writers are [`AssetSyncGuard`], created by the app crate's
//! `asset_sync::sync_set_observed` around every `sync_set` call, and [`AssetConnectGuard`], created
//! by `asset_sync::login_observed` around every `AssetSync::login` call.
//!
//! `sync_set` and `AssetSync::login` are both **private to the app crate's `asset_sync` module**, so
//! the wrappers are the only way to reach them **from anywhere else in the workspace** — an
//! unobserved sync or login added at any other call site does not compile. The limit, stated because
//! the stronger phrasing was here first (#743 review N5): code added *inside that one file* is
//! within the privacy boundary and can still call them directly. The compiler enforces the rule for
//! every caller outside it; inside it, review does.
//!
//! Each guard removes its own entry in `Drop`, so the "published but never cleared"
//! failure (an endpoint confidently reporting a long-finished sync as if it were live) cannot happen
//! on ANY exit path — success, error return, or a panic unwinding through the loader thread.
//!
//! A login's guard additionally records an [`ConnectOutcome`], because unlike `Drop` a wrapper
//! function CAN see the `Result` it is wrapping. That is what lets an agent tell "the login failed"
//! from "the login succeeded and the sync has not opened yet" once the entry is gone; for set syncs
//! the outcome remains genuinely unknown at `Drop` (see [`EndedWhat::Sync`]).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Shared handle to the asset-sync registry. Empty = **no sync is in progress**; that is a
/// different state from a sync sitting at zero progress, which is one live entry carrying a zeroed
/// [`AssetSyncPhase::Downloading`].
///
/// Constructed ONCE in `main.rs` and cloned — by `Arc` identity — into both the app (the writer)
/// and `HttpState` (the reader). Constructing a second, independent registry on either side would
/// silently sever the two and the endpoint would read "idle" forever no matter what the loaders
/// published (the shared-`Arc` trap the #616 review caught for `common_assets_failed`).
pub type AssetSyncShared = Arc<Mutex<AssetSyncSlots>>;

/// The one construction site for an [`AssetSyncShared`]. A named constructor rather than
/// `Arc::new(Mutex::new(..))` at each call site so the "there is exactly one of these" rule above
/// has something to grep for.
pub fn new_shared() -> AssetSyncShared {
    Arc::new(Mutex::new(AssetSyncSlots::default()))
}

fn lock(shared: &AssetSyncShared) -> std::sync::MutexGuard<'_, AssetSyncSlots> {
    // A loader thread panicking mid-sync poisons this mutex; the registry's invariants do not
    // depend on where that panic happened (the guard's Drop still removes the entry on unwind), so
    // recovering is strictly better than propagating the panic into the HTTP thread and taking the
    // whole observability channel down with the loader.
    shared.lock().unwrap_or_else(|e| e.into_inner())
}

/// Opaque identity of one live sync. Monotonic per registry; never reused. The point is ownership:
/// only the guard that created an entry can remove it (see the module docs).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SyncId(u64);

/// Every sync running right now, plus the last one to end.
///
/// `live` is ordered **oldest-started first**, which is a deliberate, stable order and not an
/// accident of the container: the long-running sync an agent is waiting on (and the one most likely
/// to be the wedged one) is always first, so `GET /v1/observe/asset_sync` can name a *primary*
/// without the answer hopping between syncs from poll to poll.
#[derive(Debug, Default)]
pub struct AssetSyncSlots {
    live: Vec<(SyncId, AssetSyncActivity)>,
    next_id: u64,
    last_ended: Option<EndedActivity>,
    last_login: LastLoginByOutcome,
    login_outcomes: LoginOutcomeTally,
}

impl AssetSyncSlots {
    /// Every live sync, oldest-started first.
    pub fn live(&self) -> impl ExactSizeIterator<Item = &AssetSyncActivity> {
        self.live.iter().map(|(_, a)| a)
    }

    /// The most recent activity **of any kind** to leave the registry, or `None` if none ever has.
    ///
    /// **This is one slot, and everything that ends overwrites it** — a login's verdict survives here
    /// only until the next login *or set sync* ends. Under the concurrency this client actually runs
    /// (three logins at startup, two set syncs per zone load) that is milliseconds, so a poller can
    /// and does miss a failure entirely: #743's reviewer measured a genuinely-failed login appearing
    /// in this slot in **0 of 75 samples** while three other activities cycled through it. Do not use
    /// it to answer "did a login fail" — that is what [`Self::last_login`] and
    /// [`Self::login_outcomes`] are for.
    pub fn last_ended(&self) -> Option<&EndedActivity> {
        self.last_ended.as_ref()
    }

    /// The most recent login to end with **each** outcome, retained for the life of the process —
    /// one slot per outcome. See [`LastLoginByOutcome`] for why it is one-per-outcome and not one
    /// shared "last non-success" slot (#743 review B3).
    pub fn last_login(&self) -> &LastLoginByOutcome {
        &self.last_login
    }

    /// Monotonic counts of every login that has **ended** in this process, by outcome.
    ///
    /// Counters only ever increase, so a caller polling at any cadence can answer "has a login failed
    /// at all" (`failed > 0`) and "has one failed *since my last poll*" (a delta) without needing to
    /// be looking when it happened. In-flight logins are not counted — they are in [`Self::live`].
    pub fn login_outcomes(&self) -> LoginOutcomeTally {
        self.login_outcomes
    }

    fn begin(&mut self, activity: AssetSyncActivity) -> SyncId {
        let id = SyncId(self.next_id);
        self.next_id += 1;
        // Pushed at the end, so insertion order IS start order and `live()` is oldest-first.
        self.live.push((id, activity));
        id
    }

    /// Republish `id`'s phase. A no-op if `id` is not live — a guard whose entry was somehow already
    /// removed must never be able to resurrect it as a phantom sync.
    ///
    /// Also a no-op for a `Connecting` entry: a login has no phase and must never acquire one. That
    /// is belt-and-braces — [`AssetConnectGuard`] has no `tick` method to call — but the registry
    /// should not depend on the guard type to keep an invariant it can keep itself.
    ///
    /// **Stated plainly because it looks covered and is not (#743 review N4):** the
    /// `if let AssetSyncWork::Sync` guard below is **unreachable in production and unkillable by any
    /// test.** Reaching it needs a `Connecting` entry's `SyncId` passed to `tick`, and no path can
    /// produce one — `AssetConnectGuard` exposes no `tick`, and [`SyncId`]s are never reused, so a
    /// sync guard's id can never come to name a login's entry. Removing the guard would therefore
    /// leave the whole suite green. It is kept as an invariant this type enforces for itself, not as
    /// behaviour any test asserts; treat it as unverified defence, not as tested code.
    fn tick(&mut self, id: SyncId, phase: AssetSyncPhase, published_at: Instant) {
        if let Some((_, a)) = self.live.iter_mut().find(|(i, _)| *i == id) {
            if let AssetSyncWork::Sync { phase: p, .. } = &mut a.work {
                *p = phase;
                a.published_at = published_at;
            }
        }
    }

    /// End a LOGIN's entry, with the verdict `login_observed` measured from its `Result`.
    fn end_login(&mut self, id: SyncId, outcome: ConnectOutcome) {
        self.end(id, Some(outcome));
    }

    /// End a SET SYNC's entry. Takes no outcome **because there is none to take**: the sync guard's
    /// `Drop` runs identically on success, error and unwind (see [`EndedWhat::Sync`]).
    ///
    /// This is the type-level half of #743 review N8. The previous shape was one `end(id, outcome)`
    /// with the sync guard passing [`ConnectOutcome::Unknown`] as a filler, defended by a comment
    /// arguing that [`SyncId`]s cannot alias so the filler could never reach the login arm. The
    /// argument was right, but the cost of it being wrong was that a set sync ending would
    /// **fabricate** a login failure — count an `unknown` and retain a login that never ran, under
    /// some other call's purpose. There is now no argument to be wrong about: a caller with a sync's
    /// id has no way to supply a verdict, so no set sync can write a login observable **whatever the
    /// ids do**. `end` below still has to decide what an aliased id would mean, and it decides it in
    /// the one direction that cannot invent: no tally, no retention.
    fn end_sync(&mut self, id: SyncId) {
        self.end(id, None);
    }

    /// Remove `id`'s entry — and ONLY `id`'s. `Vec::remove` preserves the relative order of the
    /// rest, so the oldest-first invariant survives a middle entry ending.
    ///
    /// `verdict` is `Some` only on [`Self::end_login`]. It is the *only* thing that unlocks the
    /// login observables; see that method and [`Self::end_sync`].
    fn end(&mut self, id: SyncId, verdict: Option<ConnectOutcome>) {
        if let Some(pos) = self.live.iter().position(|(i, _)| *i == id) {
            let (_, a) = self.live.remove(pos);
            let at = Instant::now();
            let what = match a.work {
                // A verdict passed for a Sync entry is impossible (only `end_login` can supply one,
                // and only a login guard calls it) and would be meaningless anyway — discarded.
                AssetSyncWork::Sync { set, .. } => EndedWhat::Sync { set },
                AssetSyncWork::Connecting { purpose } => {
                    // #743 review B1. `last_ended` below is a single last-writer-wins slot: the very
                    // next activity to end — a `charmodel` sync, a door set, another login — erases
                    // whatever verdict is written there. A failure recorded ONLY in that slot is
                    // therefore evidence with a lifetime of milliseconds, and the reviewer measured
                    // exactly that: a login that really did fail appeared in `last_ended` in 0 of 75
                    // polls, because three other activities ended on top of it first. An agent
                    // following the documented recipe was told "no login failed" while three had.
                    //
                    // So the verdict is ALSO recorded in two places nothing but another login of the
                    // SAME outcome can touch: a monotonic tally, and a per-outcome retained record.
                    // Neither invents information — both are written from the same measured `Result`
                    // as the verdict in `last_ended`, in one statement pair, under this one lock.
                    match verdict {
                        Some(outcome) => {
                            self.login_outcomes.record(outcome);
                            self.last_login.record(outcome, purpose.clone(), at);
                            EndedWhat::Connect { purpose, outcome }
                        }
                        // Unreachable: only `end_sync` passes `None`, and it is called only with a
                        // sync guard's own id, which can never name a `Connecting` entry (ids are a
                        // `u64` minted under this lock and never reused). Decided anyway, because
                        // "unreachable" is an argument and the durable observables must not rest on
                        // one: NOTHING is counted and NOTHING is retained. `last_ended` — the
                        // transient slot the next activity overwrites — reports it as `Unknown`,
                        // which is what it is: a login entry left with no verdict ever seen. The
                        // asymmetry is deliberate. A fabricated `login_outcomes.unknown` would be a
                        // monotonic counter that can never be walked back, and a fabricated
                        // `last_login` record would name a login that never happened.
                        None => EndedWhat::Connect { purpose, outcome: ConnectOutcome::Unknown },
                    }
                }
            };
            self.last_ended = Some(EndedActivity { what, at });
        }
    }
}

/// The most recent login to END with each outcome — **one slot per outcome, never one slot for two.**
///
/// ## Why this is a per-outcome map and not a single "last unsuccessful login" (#743 review B3)
///
/// It was a single slot holding the most recent login that did not succeed, of either kind, and the
/// docs offered it as the answer to two different questions: *which login failed* and *which login
/// panicked*. Only one of those could be true at a time. With a panic followed by a failure,
/// `login_outcomes.unknown` said a panic had happened and the one slot named the **failed** login —
/// so a caller that read the purpose out of it attributed the panic to a login that did not panic,
/// and the panicked login's identity was not recoverable from the endpoint at all. That is the same
/// "one observable, two meanings" defect this whole PR exists to fix, re-instantiated inside its own
/// fix one level down.
///
/// A guard or a doc sentence would have papered over it. This makes it unrepresentable instead, in
/// two steps:
///
/// 1. **One slot per [`ConnectOutcome`]**, reached only through [`Self::slot_mut`] — an exhaustive
///    match, so the slot is never a free choice at the call site and a new outcome variant fails to
///    compile until it is given its own slot.
/// 2. **[`RetainedLogin`] carries no outcome field.** The outcome is *which slot the record is in*.
///    A record therefore cannot claim an outcome that disagrees with the category it answers for,
///    because it has nothing to claim it with, and the encoder derives both the wire field name and
///    the wire `outcome` token from the same key.
///
/// The resulting contract is one biconditional **per outcome**, which is what the docs may promise:
/// `login_outcomes.<o> > 0` ⟺ `last_login.get(<o>).is_some()`, and when present it is a login that
/// ended with exactly `<o>`.
///
/// Within one outcome this is still a *most recent*, not a log: a second failure overwrites the
/// first failure's purpose. That is one question with one answer, so it cannot mislead the way the
/// shared slot did — and the count of both survives in [`LoginOutcomeTally`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct LastLoginByOutcome {
    succeeded: Option<RetainedLogin>,
    failed: Option<RetainedLogin>,
    unknown: Option<RetainedLogin>,
}

impl LastLoginByOutcome {
    /// The most recent login that ended with `outcome`, or `None` if none ever has — a real negative
    /// answer about *that outcome alone*, not "nothing interesting happened".
    pub fn get(&self, outcome: ConnectOutcome) -> Option<&RetainedLogin> {
        match outcome {
            ConnectOutcome::Succeeded => self.succeeded.as_ref(),
            ConnectOutcome::Failed => self.failed.as_ref(),
            ConnectOutcome::Unknown => self.unknown.as_ref(),
        }
    }

    /// Every slot, paired with the outcome it answers for, in [`ConnectOutcome::ALL`] order. This is
    /// what the HTTP encoder walks to emit the `last_login_<outcome>` records, so it is the wire's
    /// enumeration of the outcomes and not a second one beside it.
    ///
    /// **It is also the completeness pin for [`ConnectOutcome::ALL`]** — see that constant's
    /// rustdoc for the review finding that motivated it. Two compile-time checks live in this one
    /// function:
    ///
    /// 1. the `let` destructure is **exhaustive** — no `..` — so a slot added to this struct without
    ///    being listed below fails to compile here; and
    /// 2. the returned array's length is `ConnectOutcome::ALL.len()`, so listing a new slot below
    ///    without adding its outcome to `ALL` fails to compile here too.
    ///
    /// ⚠️ **[EDIT, round 5 (#755 review): the two sentences that used to stand here were false, and
    /// the counterexample is rustc's own suggested fix for the very error this function raises.]**
    /// The previous wording said an outcome given its own slot "cannot reach the wire without being
    /// in `ALL`, and cannot be in this struct while silently missing from `ALL`". Both halves are
    /// false, measured:
    ///
    /// - **What the pin actually guarantees:** a new field added to this struct forces the author to
    ///   *name it in the destructure pattern* — check 1 above — because there is no `..` to fall
    ///   back on. That much is real: E0027 fires, on the library, not only its tests.
    /// - **Naming it is not the same as serving it, and two evasions satisfy the former while
    ///   defeating the latter.** rustc's own second `help:` for that E0027 is
    ///   `refused: _` — take it verbatim, change nothing else, and the crate builds with **zero**
    ///   errors and **zero** warnings (`refused: _` is not even an `unused_variables` lint), while
    ///   `refused` sits in this struct, fully wired, silently absent from the returned array and
    ///   from `ALL`. Pointing a new variant's match arms at an *existing* slot (no new field at all)
    ///   evades check 1 the same way — see the residual on `ALL`, whose consequence is sharper than
    ///   "nothing fires": it produces a body where `last_login_failed.connecting.outcome == "failed"`
    ///   for a login that ended a *different* way.
    /// - **"Reach the wire" is not this function's claim to make.** [`EndedWhat::Connect`] stores the
    ///   raw `ConnectOutcome` and [`ConnectOutcome::as_str`] serves it into `last_ended` directly —
    ///   neither consults `ALL` nor this function. In the `refused: _` state above, a login that
    ///   ended `Refused` is absent from `login_outcomes` and `last_login_refused` **and simultaneously
    ///   present** as `last_ended.connecting.outcome == "refused"` — one body giving two answers
    ///   about one login, which is the #743 B3 defect shape (two fields disagreeing about the same
    ///   login), re-instantiated inside this fix.
    ///
    /// So: this function forces a decision at the call site. It does not force that decision to be
    /// *correct*, and it has no reach at all over the separate `as_str`-driven path to `last_ended`.
    pub fn slots(&self) -> [(ConnectOutcome, Option<&RetainedLogin>); ConnectOutcome::ALL.len()] {
        let LastLoginByOutcome { succeeded, failed, unknown } = self;
        [
            (ConnectOutcome::Succeeded, succeeded.as_ref()),
            (ConnectOutcome::Failed, failed.as_ref()),
            (ConnectOutcome::Unknown, unknown.as_ref()),
        ]
    }

    /// The one write path. Exhaustive, so which slot a record lands in is a function of the outcome
    /// and nothing else.
    fn slot_mut(&mut self, outcome: ConnectOutcome) -> &mut Option<RetainedLogin> {
        match outcome {
            ConnectOutcome::Succeeded => &mut self.succeeded,
            ConnectOutcome::Failed => &mut self.failed,
            ConnectOutcome::Unknown => &mut self.unknown,
        }
    }

    fn record(&mut self, outcome: ConnectOutcome, purpose: String, at: Instant) {
        *self.slot_mut(outcome) = Some(RetainedLogin { purpose, at });
    }
}

/// One retained login, kept for the life of the process in the [`LastLoginByOutcome`] slot for its
/// outcome.
///
/// **It deliberately carries no outcome of its own.** The outcome is which slot it is in, so the
/// record and the category it is filed under cannot disagree — see [`LastLoginByOutcome`].
#[derive(Clone, Debug, PartialEq)]
pub struct RetainedLogin {
    /// Free text naming what the login was for — see [`AssetSyncWork::Connecting`].
    pub purpose: String,
    /// When it ended. An `Instant`, turned into an age at READ time — never a cached age (#343).
    pub at: Instant,
}

/// How many logins have **ended** in this process, by outcome. Monotonic: every field only ever
/// increases, for the life of the process.
///
/// This exists because [`AssetSyncSlots::last_ended`] is a single slot that any subsequent activity
/// overwrites, so it cannot answer "did a login fail" for a caller that was not polling at the
/// instant it happened (#743 review B1, measured at 0 of 75 samples). A counter answers that at any
/// cadence, and a counter that only goes up cannot become a falsehood later: `failed > 0` means a
/// login failed, full stop, and the *delta* between two polls means one failed in between.
///
/// The three outcomes are kept separate rather than summed into "failures" because they are not the
/// same claim — [`ConnectOutcome::Unknown`] is "a panic unwound through the login", which is neither
/// of the two real answers and must not be filed as either.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoginOutcomeTally {
    pub succeeded: u64,
    pub failed: u64,
    /// A panic unwound through the login: it neither returned `Ok` nor returned `Err`.
    pub unknown: u64,
}

impl LoginOutcomeTally {
    fn record(&mut self, outcome: ConnectOutcome) {
        *self.count_mut(outcome) += 1;
    }

    fn count_mut(&mut self, outcome: ConnectOutcome) -> &mut u64 {
        match outcome {
            ConnectOutcome::Succeeded => &mut self.succeeded,
            ConnectOutcome::Failed => &mut self.failed,
            ConnectOutcome::Unknown => &mut self.unknown,
        }
    }

    /// How many logins ended with `outcome`. Keyed by the same enum as
    /// [`LastLoginByOutcome::get`], so "the count for `o`" and "the record for `o`" are addressed
    /// identically and the per-outcome biconditional between them is one loop to check, not three
    /// hand-written pairs that can drift.
    pub fn count(self, outcome: ConnectOutcome) -> u64 {
        match outcome {
            ConnectOutcome::Succeeded => self.succeeded,
            ConnectOutcome::Failed => self.failed,
            ConnectOutcome::Unknown => self.unknown,
        }
    }

    /// Logins that ended without succeeding — `failed + unknown`. The one-number answer to "is there
    /// any login this process could not complete", for a caller that does not want to add two fields
    /// and risk forgetting the second.
    ///
    /// ⚠️ **Disclosed residual (round 5, #755 review, N3 — measured, not just reasoned about).**
    /// This sum is hand-written and **not** covered by [`ConnectOutcome::ALL`] or
    /// [`LastLoginByOutcome::slots`] the way `login_outcomes`' own fields are. A fourth non-success
    /// outcome correctly wired everywhere — its own field, its own counter, all five exhaustive
    /// matches, and listed in `ALL` — still contributes **zero** here unless this line is also
    /// edited by hand. Measured: adding such a variant and calling this function returns `0` for a
    /// login that ended that way, while the dedicated counter for it is `1`.
    ///
    /// ⚠️ **[EDIT, round 6 (#755 review): "neither the compiler nor any existing test catches the
    /// omission" overstated what was measured.]** Wiring the new variant in fully (own field, own
    /// counter, all five exhaustive matches, listed in `ALL`) does trip diagnostics — four
    /// pre-existing test sites construct `LoginOutcomeTally { succeeded, failed, unknown }`
    /// positionally and fail `E0063` (missing field), and once those are patched the run reports
    /// `220 passed; 3 failed` (eqoxide-http) / `53 passed; 1 failed` (eqoxide-ipc). None of those 8
    /// diagnostics is *about* `unsuccessful()` — they fire on unrelated struct literals and
    /// assertions that were never testing this sum. The accurate claim is narrower: **nothing points
    /// at this sum**, not "nothing catches the omission."
    pub fn unsuccessful(self) -> u64 {
        self.failed + self.unknown
    }
}

/// An activity that has left the registry. This exists so an agent can distinguish "something
/// finished a moment ago" from "nothing has ever run in this process", which were previously the
/// same empty answer (the known-empty vs unknown collapse, #726 review N5).
#[derive(Clone, Debug, PartialEq)]
pub struct EndedActivity {
    pub what: EndedWhat,
    /// When it ended. An `Instant`, turned into an age at READ time — never a cached age (#343).
    pub at: Instant,
}

/// What the last-ended activity was, and — for a login only — how it went.
#[derive(Clone, Debug, PartialEq)]
pub enum EndedWhat {
    /// A `sync_set` call. **`ended` means ended, not succeeded**: [`AssetSyncGuard`]'s `Drop` runs
    /// identically on the success return, the error return and a panic unwind, and genuinely cannot
    /// tell them apart. Inventing a verdict here would be a confident falsehood; there is none.
    Sync { set: String },
    /// An asset-server login. Unlike a sync, this one DOES carry a verdict: `login_observed` wraps
    /// the call and sees its `Result`, so the outcome is measured rather than guessed. This is what
    /// answers "did the login fail?" after the entry is gone — without it, a failed login and a
    /// succeeded one are the same `active: false`, which is the #731 falsehood reappearing one
    /// moment later.
    Connect { purpose: String, outcome: ConnectOutcome },
}

/// How a login ended. Three-valued on purpose: a `Drop` that never saw a verdict must report
/// [`ConnectOutcome::Unknown`], not default to one of the two real answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectOutcome {
    Succeeded,
    Failed,
    /// The guard was dropped without a verdict — i.e. a panic unwound through the login call. The
    /// login neither succeeded nor returned an error, and saying either would be an invention.
    Unknown,
}

impl ConnectOutcome {
    /// Every outcome, once each. The encoder walks this to emit `login_outcomes` and the
    /// `last_login_<outcome>` records **in one pass**, so a category can never be counted without
    /// its record being served alongside it.
    ///
    /// ## What keeps this array complete — and what does not
    ///
    /// Written from mutations that were run, because the previous version of this paragraph was a
    /// false safety-net claim that #743's round-3 review falsified by measurement.
    ///
    /// **Adding a variant to this enum fails to compile in five exhaustive matches:**
    /// [`ConnectOutcome::as_str`], [`LastLoginByOutcome::get`], [`LastLoginByOutcome::slot_mut`],
    /// [`LoginOutcomeTally::count`] and [`LoginOutcomeTally::count_mut`]. Note what is *not* in that
    /// list: this array. Fixing all five and leaving `ALL` alone used to compile and ship a fully
    /// green suite — the review added a fourth variant, wired those sites, left it out of `ALL`, and
    /// measured `223 passed; 0 failed` / `52 passed; 0 failed`.
    ///
    /// ⚠️ **[EDIT, round 6 (#755 review): the paragraph that stood here restated, un-retracted, the
    /// same false claim already corrected at [`LastLoginByOutcome::slots`] and at the HTTP
    /// encoder — in the most-authoritative spot a reader lands, since it sits directly on `ALL`
    /// itself.]** It read:
    ///
    /// > *"That hole is closed by `LastLoginByOutcome::slots`, at compile time. A new outcome means
    /// > a new retained slot; that function destructures the slot struct exhaustively and returns an
    /// > array of length `ALL.len()`, so the new slot must be listed there (check 1) and its outcome
    /// > must be in `ALL` (check 2) before the crate builds."*
    ///
    /// Check 1 is real — a new field must be *named* at `slots()`'s destructure, or E0027 stops the
    /// library build. "Before the crate builds" is not: rustc's own suggested fix for that same
    /// E0027, `refused: _`, satisfies check 1 while never adding the slot to the returned array or to
    /// `ALL` — the crate builds clean, zero warnings, and the outcome is unserved. See
    /// [`LastLoginByOutcome::slots`]'s rustdoc for the corrected claim and its full measurement
    /// (MX-c); it is intentionally not restated here a third time.
    ///
    /// ⚠️ **[EDIT, round 7 (#755 review): "so it cannot drift from that copy again" was false the
    /// moment it was written.]** A fourth copy of the same completeness claim, compressed and
    /// un-retracted, was already sitting in this file's tests, at
    /// `the_slot_enumeration_and_all_are_the_same_list_in_the_same_order`'s rustdoc — not a new
    /// drift caused by this edit, just one this edit did not find. Not restating the correction here
    /// does not, by itself, stop a *different* sentence elsewhere from asserting the same false thing
    /// in its own words; only a grep and a retraction do that, and that fourth copy is now also
    /// retracted (see its own rustdoc). As of this commit a tree-wide grep for the completeness
    /// claim's phrasings turns up no **un-retracted** copy outside [`LastLoginByOutcome::slots`]'s
    /// rustdoc — stated as a measured fact about this commit, not a guarantee about the next one.
    ///
    /// ⚠️ **[EDIT, round 8 (#755 review, N1): "turns up none" was the wrong word, and the review's
    /// own re-grep is why.]** Run with the review's wider pattern set (`completeness pin`,
    /// `compile[- ]time pin`, `before the crate builds`, `does not build if`, `hole is closed`,
    /// `cannot reach the wire`, `fails to compile here`, `keeps .*ALL.* complete`; `*.rs`/`*.md`,
    /// `target/` excluded), a grep turns up **four** hits outside `slots()`'s rustdoc: `:541` and
    /// `:544` — the claim quoted verbatim *inside its own retraction* twelve lines above this one —
    /// `:1400`, the same thing inside the round-7 retraction, and `observe.rs:314`, which is itself
    /// a retraction EDIT. (A fifth, `renderer/src/scene.rs:283`, is unrelated prose the pattern
    /// happens to catch.) Every hit is a retraction or a quotation inside one. What was measured,
    /// and what is true, is that no *un-retracted* assertion of the claim survives anywhere —
    /// re-run at round 9 before this correction was written, not carried over from the review.
    ///
    /// **The residual, stated rather than glossed, and measured too.** A variant whose match arms
    /// are pointed at an *existing* slot and counter adds no field, so `slots` is unchanged and
    /// nothing fires. Run: that shape compiles and the suite is `223 passed; 0 failed` /
    /// `53 passed; 0 failed`. It is not a new category, it is an alias of one — the #743 B3 defect
    /// [`LastLoginByOutcome`] exists to make unrepresentable — but neither the compiler nor any test
    /// catches it here, and this doc does not claim otherwise.
    ///
    /// ⚠️ **[EDIT, round 5 (#755 review): the consequence is sharper than "nothing fires."]** A
    /// login that ends with the aliased variant produces a **self-contradicting** body, not merely a
    /// missing category. Measured by aliasing a `Refused` outcome onto `Failed`'s slot/counter
    /// (leaving [`ConnectOutcome::as_str`]'s own arm honest, since that path is independent — see
    /// below): `login_outcomes.failed == 1` and `last_login_failed.connecting.outcome == "failed"`
    /// for a login that actually ended `Refused`, while `last_ended.connecting.outcome == "refused"`
    /// for the *same* login. Two fields, both present, both confidently wrong about each other.
    ///
    /// **What the test does and does not cover.**
    /// `every_outcome_is_in_all_so_no_category_can_go_unserved` has no power over a variant that is
    /// added and left out, because nothing constructs one, so it contributes zero to both sides of
    /// its sum. The doc here used to claim it did. Its real power is over the *opposite* mutation —
    /// an outcome **removed** from this array — and even that is now the compiler's job rather than
    /// its own: shrinking `ALL` changes the length `slots` must return, so it stops the build
    /// (`expected an array with a size of 2, found one with a size of 3`) before any test runs.
    /// Measured on both trees; see the review-correction note in the PR body.
    pub const ALL: [ConnectOutcome; 3] =
        [ConnectOutcome::Succeeded, ConnectOutcome::Failed, ConnectOutcome::Unknown];

    /// The wire token, so the JSON encoder and this enum cannot drift apart.
    pub fn as_str(self) -> &'static str {
        match self {
            ConnectOutcome::Succeeded => "succeeded",
            ConnectOutcome::Failed => "failed",
            ConnectOutcome::Unknown => "unknown",
        }
    }
}

/// A read-time copy of the whole registry, so the HTTP handler holds no lock while serializing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssetSyncSnapshot {
    /// Oldest-started first — see [`AssetSyncSlots`].
    pub live: Vec<AssetSyncActivity>,
    /// The most recent activity of ANY kind to end — see [`AssetSyncSlots::last_ended`] for why this
    /// is not the field that answers "did a login fail".
    pub last_ended: Option<EndedActivity>,
    /// See [`AssetSyncSlots::last_login`] and [`LastLoginByOutcome`].
    pub last_login: LastLoginByOutcome,
    /// See [`AssetSyncSlots::login_outcomes`].
    pub login_outcomes: LoginOutcomeTally,
}

/// Copy the registry for a reader. The clone is deliberate: every age in the response is measured
/// from an `Instant` *after* the lock is released, and holding the loaders' mutex across
/// serialization would let a slow reader stall a download tick.
pub fn snapshot(shared: &AssetSyncShared) -> AssetSyncSnapshot {
    let slots = lock(shared);
    AssetSyncSnapshot {
        live: slots.live().cloned().collect(),
        last_ended: slots.last_ended().cloned(),
        last_login: slots.last_login().clone(),
        login_outcomes: slots.login_outcomes(),
    }
}

/// One in-flight asset-pipeline call, as an observer sees it: either a `sync_set` or the
/// asset-server login that precedes one (#731).
#[derive(Clone, Debug, PartialEq)]
pub struct AssetSyncActivity {
    /// WHICH kind of work this is, and the only place its kind-specific data lives.
    pub work: AssetSyncWork,
    /// When the call began. Turned into a live "has been running for N ms" at read time.
    ///
    /// Unlike everything in `phase`, this yields a number that keeps moving while the sync is
    /// wedged, and it is the ONLY duration available at all in [`AssetSyncPhase::Starting`] (a hung
    /// manifest fetch has no elapsed, no bytes and no chunks — just an age).
    pub started_at: Instant,
    /// When this sample was published. **The staleness answer, and the reason it is a timestamp
    /// rather than an age.**
    ///
    /// Every field beside it is a snapshot from the moment the producer last ticked, and the
    /// producer ticks only when a chunk completes. A download that WEDGES mid-chunk — a hung
    /// socket, an asset server that stopped answering — therefore leaves `chunks_done`, `bytes`
    /// and `elapsed` frozen at their last values with nobody left to update them. That is the shape
    /// of #343 (`connected: true` published only by a loop that had stopped running) and its repeat
    /// in #679: an observable with no live writer, still reading healthy.
    ///
    /// A guard cannot fix that by removing the entry, because a wedged sync genuinely IS still in
    /// progress — "no sync running" would be a worse answer than a stale one. What makes the stale
    /// answer honest is publishing how old it is, AND withholding the one field that is not a
    /// measurement but an assertion about *now*: see [`observed_download_rate`]. Stored as an
    /// `Instant` and turned into an age at READ time; an age computed at WRITE time is itself a
    /// value that goes stale, which is the same bug one level down.
    ///
    /// A `Connecting` entry never ticks — a login is one atomic request — so this stays at
    /// `started_at` for its whole life and its read-time age IS the time the login has been blocked.
    pub published_at: Instant,
}

impl AssetSyncActivity {
    /// The set this activity is syncing, or `None` for a login — which has no set (#731). An
    /// `Option` rather than a placeholder string: a login labelled `"zone/qeynos2"` would be found
    /// by a caller looking that set up in `syncs` and read as a transfer that has not started.
    pub fn set(&self) -> Option<&str> {
        match &self.work {
            AssetSyncWork::Sync { set, .. } => Some(set),
            AssetSyncWork::Connecting { .. } => None,
        }
    }

    /// The sync phase, or `None` for a login — a login has no phase (#731).
    pub fn phase(&self) -> Option<&AssetSyncPhase> {
        match &self.work {
            AssetSyncWork::Sync { phase, .. } => Some(phase),
            AssetSyncWork::Connecting { .. } => None,
        }
    }

    /// What this login is for, or `None` if this activity is a set sync.
    pub fn connecting_purpose(&self) -> Option<&str> {
        match &self.work {
            AssetSyncWork::Connecting { purpose } => Some(purpose),
            AssetSyncWork::Sync { .. } => None,
        }
    }
}

/// The two kinds of work the asset pipeline does, as an observer sees them.
///
/// Modelled as one enum rather than as `Option<set>` + `Option<phase>` fields on the struct for the
/// reason the whole module exists: a login carrying a set, or a phase, or (worst) transfer data
/// would be a plausible, well-formed, false answer, and an agent has no independent channel to
/// detect one. Here it is not a rule to remember — it does not typecheck.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetSyncWork {
    /// An asset-server `login()` is in flight (#731). It has no set, no phase and no transfer data,
    /// and this variant has nowhere to put any.
    ///
    /// `purpose` is **free text describing what the login is for**, e.g. `"zone load: qeynos2"`. It
    /// is deliberately NOT a set name and is deliberately not set-shaped: three of the client's four
    /// logins serve several sets (or an unbounded queue of them), so there is no set to name, and a
    /// name-shaped guess would be found by a caller looking a set up.
    Connecting { purpose: String },
    /// A `sync_set` call for one named set.
    Sync {
        /// The asset-server set being synced, verbatim — e.g. `"zone/qeynos2"`,
        /// `"zonedoors/qeynos2"`, `"common"`, `"charmodel/hum"`. This is what tells an observer WHICH
        /// sync a sample describes when loaders overlap. It is NOT an identity: two syncs of the
        /// same set can be live at once, which is why ownership is by [`SyncId`].
        set: String,
        phase: AssetSyncPhase,
    },
}

/// Where an in-flight sync is. Mirrors the producer's `asset_sync::SyncProgress` (#708) plus
/// [`AssetSyncPhase::Starting`], which covers the window this crate can see but that enum cannot:
/// between the `sync_set` call beginning and its first progress tick.
#[derive(Clone, Debug, PartialEq)]
pub enum AssetSyncPhase {
    /// The `sync_set` call has begun but the producer has not emitted a tick yet — the manifest
    /// request is in flight. Published by [`AssetSyncGuard::begin`], NOT by the producer.
    ///
    /// This variant exists so the "a sync is running" window covers the WHOLE call. Leaving the
    /// registry empty until the first producer tick would report "no sync in progress" while one
    /// demonstrably was, which is the falsehood this endpoint exists to prevent; publishing
    /// `Verifying` instead would be inventing a producer claim at the API layer.
    ///
    /// A sync that finds the set already up to date (server 304 + intact local artifacts) returns
    /// without ever ticking, so `Starting` is the only phase such a call is ever seen in.
    Starting,
    /// The producer reported `SyncProgress::Verifying`. Carries no transfer data — there is no
    /// rate to read in this phase, and none can be represented.
    Verifying,
    /// The producer reported `SyncProgress::Downloading`. The ONLY variant carrying transfer data.
    Downloading {
        /// Chunks fetched so far. `0` here is a REAL zero (a download that has not completed a
        /// chunk yet), distinct from no live entry at all (no sync).
        chunks_done: usize,
        chunks_total: usize,
        /// Cumulative bytes transferred in this downloading phase.
        bytes: u64,
        /// Time since the START of the current downloading phase only (see the producer's
        /// `fetch_and_reassemble`), so a rate derived from it is a phase-local session average and
        /// cannot be inflated by time spent verifying beforehand.
        elapsed: Duration,
    },
}

/// Minimum elapsed time before a `Downloading` tick's `bytes`/`elapsed` are considered stable enough
/// to divide into a rate. The very first tick of a phase can land well under a millisecond after
/// `Instant::now()` was captured (a tiny/local chunk, warm caches) — dividing by that would produce
/// an absurd, noisy spike rather than an honest "not enough data yet" (#708 requirement 4).
const MIN_RATE_ELAPSED: Duration = Duration::from_millis(100);

/// How old a `Downloading` sample may be before its derived rate stops being published (#726
/// review finding 2).
///
/// **Why a rate needs this and `bytes`/`chunks_done` do not.** Those are *measurements*: "as of the
/// last tick, 31 MB had moved" stays true forever, and `published_age_ms` beside it says as of when.
/// A rate is not a measurement, it is an *assertion about now* — "this transfer is moving at 1.5
/// MB/s" — and five minutes into a wedge that assertion is simply false. The reviewer measured the
/// exact case: a real load's last tick was 31,294,024 B over 20.65 s = 1,515,404 B/s; wedge it for
/// five minutes and the true phase average is ~97,600 B/s (15× lower) while the instantaneous rate
/// is zero, yet the endpoint kept serving 1,515,404.
///
/// The precedent for the fix is already in this file: under [`MIN_RATE_ELAPSED`] the client cannot
/// derive an honest rate and **omits the key** rather than emitting a plausible-looking number.
/// This is the same situation with the opposite sign, so it gets the same handling.
///
/// **Why 2 s.** From the reviewer's live capture of a healthy cold zone load on a ~1.5 MB/s link,
/// inter-tick gaps were median 41 ms, p95 185 ms, max 469 ms. 2000 ms is ~4.3× that measured
/// maximum and ~11× p95, so a link roughly four times slower than the measured one still reports a
/// rate continuously, while a genuine stall stops asserting one within about two seconds. The error
/// is deliberately asymmetric: withholding a rate that was in fact fine costs the caller a number
/// it can re-derive itself (`bytes` and `elapsed_secs` are still right there), whereas publishing
/// one that has stopped being true is the confident falsehood this endpoint exists to eliminate.
pub const MAX_RATE_SAMPLE_AGE: Duration = Duration::from_millis(2000);

/// The rate an observer may honestly be told, or the machine-readable reason there is none.
///
/// `Err` is a token that goes straight into the JSON body as `rate_unavailable`, so absence is
/// never ambiguous: the caller is told *which* rule withheld the number rather than being left to
/// re-derive it from `elapsed_secs` and `published_age_ms`.
///
/// `sample_age` is the age of the sample the `(bytes, elapsed)` came from, measured at READ time.
/// Staleness is checked first: when a sample is both too young and too stale, the fact that nothing
/// has updated it in seconds is the more serious one.
pub fn observed_download_rate(
    bytes: u64,
    elapsed: Duration,
    sample_age: Duration,
) -> Result<f64, &'static str> {
    if sample_age > MAX_RATE_SAMPLE_AGE {
        return Err("sample_too_stale");
    }
    download_rate_bytes_per_sec(bytes, elapsed).ok_or("phase_too_young")
}

/// Pure function: derives a bytes/sec rate from cumulative bytes transferred and elapsed time, or
/// `None` if `elapsed` is too small to divide by safely (see `MIN_RATE_ELAPSED`). Deliberately takes
/// plain `(u64, Duration)` rather than a `SyncProgress` so it's trivial to unit-test every edge
/// (zero elapsed, near-zero elapsed, zero bytes) without constructing a whole sync pipeline.
///
/// Defined here rather than in the app crate's `asset_sync` (which re-exports it verbatim, so every
/// #708 call site is unchanged) because BOTH the loading-screen HUD and `GET /v1/observe/asset_sync`
/// must derive the rate the same way — and `eqoxide-http` cannot reach up into the app crate. One
/// definition, one 100 ms threshold, two readers (#715).
///
/// The HUD calls this directly and correctly: it is redrawn from the tick that produced the sample,
/// so its rate is never stale. Only the API — which can be polled long after the last tick — needs
/// the extra staleness rule in [`observed_download_rate`].
pub fn download_rate_bytes_per_sec(bytes: u64, elapsed: Duration) -> Option<f64> {
    if elapsed < MIN_RATE_ELAPSED {
        return None;
    }
    Some(bytes as f64 / elapsed.as_secs_f64())
}

/// RAII writer for [`AssetSyncShared`]: registers an entry publishing [`AssetSyncPhase::Starting`]
/// on construction, [`Self::tick`] for each producer phase, and removes **its own** entry in `Drop`.
///
/// `Drop` — not an explicit call at the end of the happy path — is the point. The recurring defect
/// class here is an observable that is written but never cleared, which turns the endpoint into a
/// confident report of a sync that finished long ago. A guard clears on the success return, on the
/// error return, and on a panic unwinding out of the loader thread, with no code path left to
/// forget. Owning its entry by [`SyncId`] is the other half: a guard can only ever remove itself,
/// so a short nested sync finishing cannot blank a long one that is still running.
pub struct AssetSyncGuard {
    shared: AssetSyncShared,
    id: SyncId,
}

impl AssetSyncGuard {
    /// Starts observing a sync of `set`, publishing [`AssetSyncPhase::Starting`] immediately.
    pub fn begin(shared: &AssetSyncShared, set: &str) -> Self {
        // The clock is read INSIDE the lock (#726 review round 2, nit 4). `live()` promises
        // oldest-started order and delivers insertion order; those are the same thing only if no
        // other thread can insert between our timestamp and our push. Stamping outside the lock left
        // a microseconds-wide window in which two racing loaders could be listed in the opposite
        // order to their `started_at`, making the ordering claim approximately rather than exactly
        // true. Nothing was lost when it happened — both syncs were still listed — but "insertion
        // order IS start order" is the sentence the ordering contract rests on, so it should be a
        // fact rather than a near-certainty.
        let mut slots = lock(shared);
        let now = Instant::now();
        let id = slots.begin(AssetSyncActivity {
            work: AssetSyncWork::Sync { set: set.to_string(), phase: AssetSyncPhase::Starting },
            started_at: now,
            published_at: now,
        });
        drop(slots);
        Self { shared: shared.clone(), id }
    }

    /// Publishes a producer phase for this guard's sync, stamped now.
    pub fn tick(&self, phase: AssetSyncPhase) {
        self.tick_stamped(phase, Instant::now());
    }

    /// [`Self::tick`] with an explicit publish timestamp.
    ///
    /// Exists so tests can stage a WEDGED sync — a sample whose last tick was minutes ago — without
    /// sleeping for minutes. Production code always uses [`Self::tick`]; a stamp is never anything
    /// but `Instant::now()` on a real tick, because the whole staleness contract rests on the stamp
    /// being the moment the producer actually spoke.
    pub fn tick_stamped(&self, phase: AssetSyncPhase, published_at: Instant) {
        lock(&self.shared).tick(self.id, phase, published_at);
    }
}

impl Drop for AssetSyncGuard {
    fn drop(&mut self) {
        // A set sync's outcome is genuinely unknown at `Drop` (see `EndedWhat::Sync`), and #743
        // review N8 is now answered by there being no argument here to get wrong: `end_sync` takes
        // no verdict, so this call cannot write `login_outcomes` or `last_login` however the ids
        // behave. What that would have cost is recorded on `AssetSyncSlots::end_sync`, and the
        // aliased-id case is asserted in
        // `a_set_syncs_end_cannot_fabricate_a_login_observable_even_with_an_aliased_id`.
        lock(&self.shared).end_sync(self.id);
    }
}

/// RAII writer for an asset-server LOGIN (#731): registers an [`AssetSyncWork::Connecting`] entry on
/// construction and removes **its own** entry in `Drop`, exactly like [`AssetSyncGuard`].
///
/// A separate type rather than a mode flag on that guard, because the two support different
/// operations and conflating them is how a login would end up carrying a download rate. This guard
/// has **no `tick`** — a login publishes once, at `begin`, and there is nothing to republish — and
/// it has a [`Self::finish`] the sync guard cannot have, because `login_observed` sees the login's
/// `Result` and `sync_set_observed`'s `Drop` does not see a sync's.
pub struct AssetConnectGuard {
    shared: AssetSyncShared,
    id: SyncId,
    outcome: ConnectOutcome,
}

impl AssetConnectGuard {
    /// Starts observing a login. `purpose` is free text (see [`AssetSyncWork::Connecting`]).
    pub fn begin(shared: &AssetSyncShared, purpose: &str) -> Self {
        Self::begin_stamped(shared, purpose, Instant::now())
    }

    /// [`Self::begin`] with an explicit start timestamp.
    ///
    /// Exists so tests can stage a login that has been BLOCKED for minutes without sleeping for
    /// minutes — the same role, and the same caveat, as [`AssetSyncGuard::tick_stamped`]. Production
    /// code always uses [`Self::begin`]: a login has exactly one sample and it is taken when the
    /// call actually starts, because the whole staleness contract rests on that.
    ///
    /// A login never ticks, so this sets `started_at` AND `published_at` — for this variant they are
    /// the same instant by construction.
    pub fn begin_stamped(shared: &AssetSyncShared, purpose: &str, now: Instant) -> Self {
        // Clock read inside the lock, for the same ordering reason as `AssetSyncGuard::begin`.
        let mut slots = lock(shared);
        let id = slots.begin(AssetSyncActivity {
            work: AssetSyncWork::Connecting { purpose: purpose.to_string() },
            started_at: now,
            published_at: now,
        });
        drop(slots);
        Self { shared: shared.clone(), id, outcome: ConnectOutcome::Unknown }
    }

    /// Record the login's verdict and end the entry. Consumes the guard, so the entry is removed by
    /// the `Drop` that immediately follows — one removal path, not two.
    ///
    /// Not calling this (a panic unwinding out of the login) leaves the outcome
    /// [`ConnectOutcome::Unknown`], which is the honest answer: the call neither returned `Ok` nor
    /// returned `Err`.
    pub fn finish(mut self, outcome: ConnectOutcome) {
        self.outcome = outcome;
    }
}

impl Drop for AssetConnectGuard {
    fn drop(&mut self) {
        lock(&self.shared).end_login(self.id, self.outcome);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live SET SYNCS, by set name. A login has no set, so it is not listed here — which is
    /// what the `set()` accessor being an `Option` buys.
    fn live_sets(s: &AssetSyncShared) -> Vec<String> {
        lock(s).live().filter_map(|a| a.set().map(str::to_string)).collect()
    }

    /// Every live entry, as `"set"` or `"connecting:<purpose>"` — the whole registry, not just syncs.
    fn live_all(s: &AssetSyncShared) -> Vec<String> {
        lock(s).live().map(|a| match &a.work {
            AssetSyncWork::Sync { set, .. } => set.clone(),
            AssetSyncWork::Connecting { purpose } => format!("connecting:{purpose}"),
        }).collect()
    }

    fn dl(chunks_done: usize) -> AssetSyncPhase {
        AssetSyncPhase::Downloading {
            chunks_done, chunks_total: 374, bytes: 31_294_024, elapsed: Duration::from_secs(20),
        }
    }

    fn phase_of(s: &AssetSyncShared, set: &str) -> Option<AssetSyncPhase> {
        lock(s).live().find(|a| a.set() == Some(set)).and_then(|a| a.phase().cloned())
    }

    #[test]
    fn guard_clears_the_slot_when_it_drops() {
        // The "published but never cleared" trap: an endpoint that keeps reporting a finished sync
        // as if it were live. Dropping the guard is the ONLY completion signal, so it must clear.
        let s = new_shared();
        {
            let g = AssetSyncGuard::begin(&s, "zone/qeynos2");
            g.tick(dl(3));
            assert_eq!(live_sets(&s), ["zone/qeynos2"], "a live sync must be observable while it runs");
        }
        assert!(live_sets(&s).is_empty(),
            "the registry must read 'no sync in progress' once the sync is over — a stale completed \
             sync reported as live is worse than reporting nothing");
    }

    #[test]
    fn guard_clears_even_when_the_thread_panics() {
        // A loader thread panicking is one of the paths #595/#616 already had to backstop. An
        // explicit end-of-happy-path clear would be skipped by the unwind and freeze the endpoint
        // on the phase the sync died in, forever.
        let s = new_shared();
        let s2 = s.clone();
        let r = std::panic::catch_unwind(move || {
            let _g = AssetSyncGuard::begin(&s2, "zone/qeynos2");
            panic!("loader thread died mid-sync");
        });
        assert!(r.is_err(), "the test must actually have panicked");
        assert!(live_sets(&s).is_empty(), "an unwind must still leave the registry honest");
    }

    /// #726 review finding 1, the direction the original single slot got RIGHT — an OLDER loader
    /// finishing while a newer one runs.
    #[test]
    fn a_finishing_sync_does_not_erase_a_different_sync_that_is_still_running() {
        // Zone change: the previous zone's loader is still alive and finishes at some arbitrary
        // later point. Clearing unconditionally there would blank out the CURRENT zone's live
        // progress and report "no sync in progress" while the agent is waiting on that very load.
        let s = new_shared();
        let old = AssetSyncGuard::begin(&s, "zone/qeynos2");
        let _new = AssetSyncGuard::begin(&s, "zone/freportw");
        drop(old);
        assert_eq!(live_sets(&s), ["zone/freportw"],
            "an older loader finishing must not erase the newer sync that is still in flight");
    }

    /// #726 review finding 1, THE BLOCKING DIRECTION — a short sync that begins *and ends* inside a
    /// long one. The client runs this routinely: the model-sync worker fetches `charmodel/<key>` on
    /// its own thread while the zone loader is mid-download. Under the old set-scoped single slot
    /// the nested guard took the slot on `begin`, so its `Drop` saw its own set published, cleared,
    /// and the endpoint answered "no asset sync is running" while a 31 MB zone download was still
    /// in flight.
    #[test]
    fn reviewer_a_short_nested_sync_finishing_blanks_the_long_one_still_running() {
        let s = new_shared();
        let long = AssetSyncGuard::begin(&s, "zone/neriakc");
        long.tick(dl(120));
        {
            let _short = AssetSyncGuard::begin(&s, "charmodel/hum");
        } // the nested sync begins AND ends entirely inside the outer one
        assert!(live_sets(&s).iter().any(|set| set == "zone/neriakc"),
            "zone/neriakc is STILL DOWNLOADING, but the endpoint now reports 'no asset sync is \
             running' — a confident falsehood, not a stale truth");
        assert_eq!(phase_of(&s, "zone/neriakc"), Some(dl(120)),
            "…and its progress must survive intact, not be reset to Starting");
        assert_eq!(live_sets(&s), ["zone/neriakc"], "the nested sync itself is over and must be gone");
    }

    /// The worst case the review identified: the outer sync is wedged in `Starting` — a hung
    /// manifest request — so it has published exactly ONCE, at `begin`, and will never publish
    /// again. Under the old slot a nested sync deleted that single sample permanently and the
    /// endpoint read idle for the entire wedge: the wedge this endpoint exists to detect became
    /// invisible, and the endpoint served the healthiest possible answer while it happened.
    #[test]
    fn reviewer_a_nested_sync_blanks_a_zone_sync_wedged_in_starting_permanently() {
        let s = new_shared();
        let _wedged = AssetSyncGuard::begin(&s, "zone/neriakc"); // never ticks again
        {
            let _short = AssetSyncGuard::begin(&s, "charmodel/hum");
        }
        assert_eq!(live_sets(&s), ["zone/neriakc"],
            "a sync wedged in Starting has one sample and no writer left — deleting it makes the \
             wedge unobservable for as long as it lasts");
        assert_eq!(phase_of(&s, "zone/neriakc"), Some(AssetSyncPhase::Starting));
    }

    /// Why ownership is by [`SyncId`] and not by `set`. A set-keyed map fixes the measured nesting
    /// case but not this one, where the same defect reappears between two syncs of the SAME set (a
    /// re-zone into the zone already loading; the same race model requested twice).
    #[test]
    fn two_concurrent_syncs_of_the_same_set_own_their_entries_independently() {
        let s = new_shared();
        let first = AssetSyncGuard::begin(&s, "charmodel/hum");
        first.tick(dl(1));
        let second = AssetSyncGuard::begin(&s, "charmodel/hum");
        assert_eq!(live_sets(&s), ["charmodel/hum", "charmodel/hum"],
            "two overlapping syncs of one set are two syncs, not one that overwrote the other");
        drop(second);
        assert_eq!(live_sets(&s), ["charmodel/hum"], "exactly one must remain");
        assert_eq!(phase_of(&s, "charmodel/hum"), Some(dl(1)),
            "and it must be the FIRST one, with its own progress — not the survivor of an eviction");
        drop(first);
        assert!(live_sets(&s).is_empty());
    }

    #[test]
    fn live_syncs_are_ordered_oldest_first_even_after_a_middle_one_ends() {
        // The endpoint names `live[0]` as the primary sync. That is only a stable, meaningful
        // choice if the order is start order and stays start order as entries come and go.
        let s = new_shared();
        let a = AssetSyncGuard::begin(&s, "zone/neriakc");
        let b = AssetSyncGuard::begin(&s, "zonedoors/neriakc");
        let _c = AssetSyncGuard::begin(&s, "charmodel/hum");
        assert_eq!(live_sets(&s), ["zone/neriakc", "zonedoors/neriakc", "charmodel/hum"]);
        drop(b);
        assert_eq!(live_sets(&s), ["zone/neriakc", "charmodel/hum"],
            "removing a middle entry must not reorder the rest");
        drop(a);
        assert_eq!(live_sets(&s), ["charmodel/hum"]);
    }

    #[test]
    fn begin_publishes_starting_so_the_pre_tick_window_is_not_reported_as_idle() {
        // Between the sync_set call starting and its first producer tick the manifest request is in
        // flight. Leaving the registry empty there would report "no sync in progress" while one was.
        let s = new_shared();
        let _g = AssetSyncGuard::begin(&s, "common");
        let slots = lock(&s);
        let a = slots.live().next().expect("a sync that has begun must be observable before its first tick");
        assert_eq!(a.set(), Some("common"));
        assert_eq!(a.phase(), Some(&AssetSyncPhase::Starting));
    }

    /// #726 review N5 — "nothing is syncing" and "nothing has EVER synced" were the same answer.
    #[test]
    fn an_empty_registry_still_says_whether_a_sync_has_ever_run() {
        let s = new_shared();
        assert!(lock(&s).last_ended().is_none(),
            "before any sync has run there is nothing to report — that is 'unknown', and it must \
             not be dressed up as a completion");
        {
            let _g = AssetSyncGuard::begin(&s, "zone/neriakc");
        }
        let slots = lock(&s);
        let ended = slots.last_ended().expect("a sync that ran and ended must be distinguishable from one that never ran");
        assert_eq!(ended.what, EndedWhat::Sync { set: "zone/neriakc".into() });
    }

    #[test]
    fn a_ticks_phase_cannot_resurrect_a_sync_that_already_ended() {
        // Defensive: `tick` finds nothing for a removed id and must stay a no-op rather than
        // re-inserting a phantom entry that no guard owns and nothing will ever clear.
        let s = new_shared();
        let g = AssetSyncGuard::begin(&s, "common");
        lock(&s).end_sync(g.id);
        g.tick(dl(1));
        assert!(live_sets(&s).is_empty(), "a tick after the end must not republish the sync");
    }

    // ── #731: the login window ──────────────────────────────────────────────────────────────────

    /// #731, the bug itself at the registry level. A blocked `AssetSync::login()` used to leave the
    /// registry EMPTY, so `GET /v1/observe/asset_sync` answered "no asset sync is running" while a
    /// loader thread sat inside it. An agent polling that concludes the client is idle and healthy;
    /// it is neither, and it has no other channel to find out.
    #[test]
    fn a_login_in_flight_is_observable_rather_than_reading_as_idle() {
        let s = new_shared();
        let _g = AssetConnectGuard::begin(&s, "zone load: qeynos2");
        let slots = lock(&s);
        assert_eq!(slots.live().len(), 1,
            "a client blocked inside login() must not read as idle — that is #731");
        let a = slots.live().next().unwrap();
        assert_eq!(a.connecting_purpose(), Some("zone load: qeynos2"));
    }

    /// The subtler falsehood the naive fix would have introduced: a login reported through the sync
    /// shape, i.e. as a transfer sitting at 0 bytes. It is not a transfer at all, and here it cannot
    /// become one — `set()` and `phase()` are `None` by construction, and there is no variant field
    /// that could hold bytes, chunks or a rate.
    #[test]
    fn a_login_carries_no_set_no_phase_and_no_transfer_data() {
        let s = new_shared();
        let _g = AssetConnectGuard::begin(&s, "common asset load");
        let slots = lock(&s);
        let a = slots.live().next().unwrap();
        assert_eq!(a.set(), None,
            "a login is not a sync of any set — naming one would be found by a caller looking that \
             set up in `syncs` and read as a transfer that had started");
        assert_eq!(a.phase(), None, "a login has no phase");
        assert!(matches!(a.work, AssetSyncWork::Connecting { .. }));
        // …and it is not listed among the SET syncs at all.
        drop(slots);
        assert!(live_sets(&s).is_empty());
        assert_eq!(live_all(&s), ["connecting:common asset load"]);
    }

    /// A login never ticks, so `published_at` stays at `started_at` and its read-time age IS how
    /// long the login has been blocked. That is what makes a hung login feed the endpoint's
    /// `stalest_published_age_ms` with no special case.
    #[test]
    fn a_logins_sample_is_never_republished_so_its_age_measures_the_block() {
        let s = new_shared();
        let _g = AssetConnectGuard::begin(&s, "zone load: neriakc");
        let slots = lock(&s);
        let a = slots.live().next().unwrap();
        assert_eq!(a.published_at, a.started_at,
            "a login publishes exactly once, at begin — the same shape as a sync wedged in \
             Starting, and the reason its age is a wedge signal");
    }

    /// The guard clears on every exit path, exactly like the sync guard — including the one a
    /// hand-written "clear after the happy path" would miss.
    #[test]
    fn a_login_guard_clears_on_success_on_failure_and_on_a_panic() {
        let s = new_shared();
        AssetConnectGuard::begin(&s, "p").finish(ConnectOutcome::Succeeded);
        assert!(live_all(&s).is_empty(), "success must clear");
        AssetConnectGuard::begin(&s, "p").finish(ConnectOutcome::Failed);
        assert!(live_all(&s).is_empty(), "failure must clear");

        let s2 = s.clone();
        let r = std::panic::catch_unwind(move || {
            let _g = AssetConnectGuard::begin(&s2, "p");
            panic!("login thread died");
        });
        assert!(r.is_err(), "the test must actually have panicked");
        assert!(live_all(&s).is_empty(), "an unwind must still leave the registry honest");
    }

    /// #731's failure question: an agent must be able to tell "not started" from "failed" from
    /// "succeeded". A failed login that simply returns the registry to empty reproduces the original
    /// falsehood one moment later. `login_observed` sees the `Result`, so unlike a sync's `Drop` the
    /// verdict here is MEASURED — and `Unknown` exists so a panic is not silently filed as a failure.
    #[test]
    fn a_finished_login_records_which_way_it_went() {
        let s = new_shared();
        assert!(lock(&s).last_ended().is_none(), "nothing has run: not started");

        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Failed);
        assert_eq!(lock(&s).last_ended().unwrap().what,
            EndedWhat::Connect { purpose: "common asset load".into(), outcome: ConnectOutcome::Failed },
            "a failed login must be distinguishable from a successful one after the fact");

        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Succeeded);
        assert_eq!(lock(&s).last_ended().unwrap().what,
            EndedWhat::Connect { purpose: "common asset load".into(), outcome: ConnectOutcome::Succeeded });

        // A panic unwinding out of the login: neither Ok nor Err was returned, and claiming either
        // would be an invention.
        let s2 = s.clone();
        let _ = std::panic::catch_unwind(move || {
            let _g = AssetConnectGuard::begin(&s2, "common asset load");
            panic!("boom");
        });
        assert_eq!(lock(&s).last_ended().unwrap().what,
            EndedWhat::Connect { purpose: "common asset load".into(), outcome: ConnectOutcome::Unknown },
            "a guard dropped without a verdict must say Unknown, not default to a real answer");
    }

    /// **#743 review B1, as the reviewer measured it.** `last_ended` is one slot that every ending
    /// activity overwrites, so a login's verdict there has a lifetime of milliseconds. On a live run
    /// where all four logins failed, the reviewer polled 75 times at 1.5 s and the genuinely-failed
    /// `common asset load` login appeared in `last_ended` **0 times** — three other activities ended
    /// on top of it first. An agent following the then-documented recipe read "no login failed" while
    /// three had, which is #731's own shape (absence read as a negative answer) surviving inside the
    /// failure path `ConnectOutcome` was added to expose.
    ///
    /// This replays that exact transition sequence and pins the fix: the failure must still be
    /// answerable after everything else has ended on top of it.
    #[test]
    fn a_login_failure_outlives_every_later_activity_that_overwrites_last_ended() {
        let s = new_shared();

        // The measured sequence. `common asset load` fails FIRST and is then buried.
        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Failed);
        assert_eq!(lock(&s).login_outcomes().failed, 1);

        AssetConnectGuard::begin(&s, "startup game data (gamedata, gameequip)")
            .finish(ConnectOutcome::Failed);
        AssetConnectGuard::begin(&s, "model-sync worker (charmodel sets)")
            .finish(ConnectOutcome::Failed);
        // …and a SET SYNC ending is enough on its own: the slot is not login-only. On a healthy
        // client this is the common case — every zone load ends two syncs.
        drop(AssetSyncGuard::begin(&s, "zonedoors/neriakc"));

        // The measured falsehood, pinned as the behaviour it actually is: `last_ended` no longer
        // knows anything about a login at all.
        assert_eq!(lock(&s).last_ended().unwrap().what,
            EndedWhat::Sync { set: "zonedoors/neriakc".into() },
            "last_ended is a single slot; this documents that it really is destroyed, so anyone \
             reading it as a failure history is reading a field that does not do that");

        // …and the fix: the failure is still answerable, at any cadence, by both new routes.
        let slots = lock(&s);
        assert_eq!(slots.login_outcomes().failed, 3,
            "three logins failed and nothing may walk that back");
        assert_eq!(slots.login_outcomes().unsuccessful(), 3);
        assert_eq!(slots.last_login().get(ConnectOutcome::Failed).map(|r| r.purpose.clone()),
            Some("model-sync worker (charmodel sets)".into()),
            "a set sync ending must not erase the most recent login failure");
    }

    /// The retention rule, in the direction that would quietly undo it: a *successful* login must not
    /// clear the record of an earlier failure, and the counters must never decrease. A "last login
    /// outcome" field would pass every assertion in the test above and still lose the failure here.
    #[test]
    fn a_later_success_neither_erases_a_failure_nor_decrements_anything() {
        let s = new_shared();
        for o in ConnectOutcome::ALL {
            assert!(lock(&s).last_login().get(o).is_none(),
                "no login has ended {o:?}: absent is the honest answer, and it is a real negative — \
                 not 'unknown'");
        }
        assert_eq!(lock(&s).login_outcomes(), LoginOutcomeTally::default());

        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Failed);
        AssetConnectGuard::begin(&s, "zone load: neriakc").finish(ConnectOutcome::Succeeded);
        AssetConnectGuard::begin(&s, "zone load: qeynos2").finish(ConnectOutcome::Succeeded);

        let slots = lock(&s);
        assert_eq!(slots.login_outcomes(),
            LoginOutcomeTally { succeeded: 2, failed: 1, unknown: 0 },
            "counters are monotonic per outcome; a success is not the absence of a failure");
        assert_eq!(slots.last_login().get(ConnectOutcome::Failed).map(|r| r.purpose.clone()),
            Some("common asset load".into()),
            "two later successes must not make an earlier failure unobservable");
        assert_eq!(slots.last_login().get(ConnectOutcome::Succeeded).map(|r| r.purpose.clone()),
            Some("zone load: qeynos2".into()),
            "…and the successes land in their OWN slot, where they displace nothing");
        assert_eq!(slots.last_ended().unwrap().what,
            EndedWhat::Connect { purpose: "zone load: qeynos2".into(),
                                 outcome: ConnectOutcome::Succeeded },
            "…while last_ended honestly reports the most recent thing, which is the success");
    }

    /// A panic unwinding through a login did not succeed, so it must be retained as a non-success —
    /// filing it as a success by omission is the same falsehood at smaller scale. It is kept
    /// SEPARATE from `failed` because "neither Ok nor Err" is not the same claim as "returned Err".
    ///
    /// **Scope, stated because round 2's version of this test was cited as coverage it did not
    /// provide (#743 review B3):** the later activity here is a *success*, and a success has its own
    /// slot, so this shows retention against something that was never going to disturb it. The
    /// interesting neighbour — a non-success of the OTHER kind, which under round 2's single shared
    /// slot really did destroy this one — is
    /// [`a_failure_and_a_panic_never_compete_for_a_slot_so_neither_can_hide_the_other`].
    #[test]
    fn a_panic_through_a_login_is_retained_as_a_non_success_but_not_as_a_failure() {
        let s = new_shared();
        let s2 = s.clone();
        let _ = std::panic::catch_unwind(move || {
            let _g = AssetConnectGuard::begin(&s2, "model-sync worker (charmodel sets)");
            panic!("boom");
        });
        // Bury it under a later successful login, as any real client would.
        AssetConnectGuard::begin(&s, "zone load: neriakc").finish(ConnectOutcome::Succeeded);

        let slots = lock(&s);
        assert_eq!(slots.login_outcomes(),
            LoginOutcomeTally { succeeded: 1, failed: 0, unknown: 1 },
            "a panic is counted, and counted as its own outcome");
        assert_eq!(slots.login_outcomes().unsuccessful(), 1,
            "`unsuccessful` must include unknown, or 'could every login complete?' answers yes");
        assert_eq!(slots.last_login().get(ConnectOutcome::Unknown).map(|r| r.purpose.clone()),
            Some("model-sync worker (charmodel sets)".into()),
            "…and it is retained in its OWN outcome's slot, not relabelled as a failure");
        assert!(slots.last_login().get(ConnectOutcome::Failed).is_none(),
            "…and NOT under `failed`, which is the counter an alarm is most likely wired to");
    }

    /// **#743 review B3 — the reviewer's two probes, at the registry.**
    ///
    /// Round 2 kept the most recent non-success of EITHER kind in one `last_login_failure` slot and
    /// documented it as the answer to two questions. The reviewer's probes ran both orderings and
    /// both were RED: with a panic then a failure, `unknown == 1` while the slot named the FAILED
    /// login — so a caller reading the purpose on the strength of that counter attributed the panic
    /// to a login that did not panic, and the panicked login's identity was recoverable nowhere.
    ///
    /// Both orderings are reproduced here with real `catch_unwind` panics through a real guard, and
    /// both now hold, because [`LastLoginByOutcome`] gives each outcome its own slot. Losing the
    /// *earlier* login's purpose is no longer a limitation to be documented — it does not happen.
    ///
    /// The remaining loss is one question with one answer (a second failure replaces the first
    /// failure's purpose, both still counted), which cannot mislead the way a shared slot did.
    #[test]
    fn a_failure_and_a_panic_never_compete_for_a_slot_so_neither_can_hide_the_other() {
        fn panic_through_a_login(s: &AssetSyncShared, purpose: &'static str) {
            // The panic hook is process-global and these tests run in parallel, so it is left alone:
            // the "boom" backtrace on stderr is expected output, not a failing test.
            let s2 = s.clone();
            let r = std::panic::catch_unwind(move || {
                let _g = AssetConnectGuard::begin(&s2, purpose);
                panic!("boom");
            });
            assert!(r.is_err(), "the fixture must actually have panicked");
        }
        fn purpose_of(s: &AssetSyncShared, o: ConnectOutcome) -> Option<String> {
            lock(s).last_login().get(o).map(|r| r.purpose.clone())
        }

        // Probe 1: panic first, then a failure. Both must be fully recoverable.
        let s = new_shared();
        panic_through_a_login(&s, "model-sync worker (charmodel sets)");
        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Failed);
        assert_eq!(lock(&s).login_outcomes(),
            LoginOutcomeTally { succeeded: 0, failed: 1, unknown: 1 },
            "both are counted, separately, and neither count is walked back");
        assert_eq!(purpose_of(&s, ConnectOutcome::Unknown),
            Some("model-sync worker (charmodel sets)".into()),
            "`unknown == 1`, so the record for `unknown` must be the login that PANICKED — round 2 \
             returned the failed login's purpose here, which is the defect");
        assert_eq!(purpose_of(&s, ConnectOutcome::Failed), Some("common asset load".into()),
            "…and the later failure did not overwrite it, because it never shared the slot");

        // Probe 2, the mirror. Round 2 lost the FAILURE's identity in this ordering.
        let s = new_shared();
        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Failed);
        panic_through_a_login(&s, "zone load: neriakc");
        assert_eq!(lock(&s).login_outcomes(),
            LoginOutcomeTally { succeeded: 0, failed: 1, unknown: 1 });
        assert_eq!(purpose_of(&s, ConnectOutcome::Failed), Some("common asset load".into()),
            "`failed == 1`, and the record for `failed` is the login that FAILED — a later panic \
             cannot displace it");
        assert_eq!(purpose_of(&s, ConnectOutcome::Unknown), Some("zone load: neriakc".into()));
        assert_eq!(lock(&s).login_outcomes().unsuccessful(), 2,
            "two logins did not complete, and BOTH are named — not just the later one");
    }

    /// **#743 round-3 review B1.**
    ///
    /// ⚠️ **[EDIT, round 7 (#755 review): this used to open "[`LastLoginByOutcome::slots`] is the
    /// compile-time pin that keeps [`ConnectOutcome::ALL`] complete" — a fourth, compressed,
    /// un-retracted copy of the same completeness claim already corrected at `slots()`'s own
    /// rustdoc, at the HTTP encoder, and at `ALL` itself. Found by a round-7 tree-wide grep, after
    /// round 6 had claimed the correction was down to one copy — it was not; see
    /// [`ConnectOutcome::ALL`]'s rustdoc for that correction.]** See [`LastLoginByOutcome::slots`]'s
    /// own rustdoc for what is actually true, and measured, about completeness.
    ///
    /// What still holds: `slots()` IS what the HTTP encoder walks, and it is a second enumeration of
    /// the outcomes — independent of whether either enumeration is *complete*, two enumerations that
    /// can disagree in *contents or order* are how the defect this PR is about got in. The compiler
    /// pins their *length*; this test pins their *contents and order*, so `slots()[i]` and `ALL[i]`
    /// are the same outcome and a caller may zip them.
    ///
    /// MUTATION-CHECK: swap two entries in `slots`, or point one at another outcome's field, and
    /// this goes RED.
    #[test]
    fn the_slot_enumeration_and_all_are_the_same_list_in_the_same_order() {
        let s = new_shared();
        // One login per outcome, each with a distinguishable purpose, so a mis-ordered pairing shows
        // up as a record filed under the wrong outcome rather than as a length mismatch alone.
        AssetConnectGuard::begin(&s, "ok").finish(ConnectOutcome::Succeeded);
        AssetConnectGuard::begin(&s, "bad").finish(ConnectOutcome::Failed);
        drop(AssetConnectGuard::begin(&s, "panicked"));

        let slots = lock(&s);
        let last = slots.last_login();
        let enumerated: Vec<ConnectOutcome> = last.slots().iter().map(|(o, _)| *o).collect();
        assert_eq!(enumerated, ConnectOutcome::ALL.to_vec(),
            "`slots()` and `ALL` must be the same outcomes in the same order — they are two \
             enumerations of one alphabet and the encoder pairs them positionally");
        for (o, record) in last.slots() {
            assert_eq!(record, last.get(o),
                "`slots()` and `get()` must return the same record for {o:?} — one is an exhaustive \
                 destructure of the fields, the other an exhaustive match on the outcome, and a \
                 disagreement means a record is filed under an outcome that is not its own");
        }
    }

    /// **#755 round 6 review — the fourth evasion, none of the other pins reach.** Every check above
    /// pins *whether* a slot exists and *where* it is walked from; none of them pins the *wire token*
    /// [`ConnectOutcome::as_str`] hands out. The HTTP encoder keys a `serde_json::Map` by that token
    /// (`counts.insert(token, …)`, `obj.insert(format!("last_login_{token}"), …)`), so two outcomes
    /// sharing one token do not both land on the wire — the second `insert` silently overwrites the
    /// first. A variant added with every OTHER pin satisfied (own field, own counter, in `slots()`,
    /// in `ALL`, all five exhaustive matches) but with an `as_str()` arm copy-pasted from an existing
    /// one compiles clean, every existing test stays green, and produces a body where a whole outcome
    /// category is missing from `login_outcomes` while a login that ended that way is reported as
    /// having ended the OTHER way — not merely absent, but affirmatively mislabelled. That is the
    /// #743 B3 shape again, and this is the only test in this module that can see it.
    ///
    /// **Scope, stated precisely (round 7, non-blocking review note).** This test's power is over
    /// [`ConnectOutcome::ALL`], not the enum in general: in the `refused: _` evasion state
    /// [`LastLoginByOutcome::slots`]'s rustdoc documents as reachable with a clean build, a variant
    /// left OUT of `ALL` still reaches `last_ended` via `as_str()` (that path never consults `ALL`),
    /// so a colliding token on such a variant is unseen here. That gap is a reading of the three
    /// lines below, not a separate measurement — flagged as a hypothesis, not re-run as its own
    /// mutation.
    ///
    /// MUTATION-CHECK, run (not reasoned about) at #755 round 6, RE-RUN at round 7 after the review
    /// found the round-6 claim below wrong, and RE-RUN AGAIN at round 9: added a fifth `Refused`
    /// variant end-to-end — own field on `LastLoginByOutcome`, own counter on `LoginOutcomeTally`,
    /// its own entry in `slots()`, listed **first** in `ALL` (the round-7 review's ordering, chosen
    /// as the one most favourable to the round-6 claim), all five exhaustive matches wired — with
    /// `as_str()` returning `"failed"` for it (copy-pasted from `Failed`'s arm) instead of
    /// `"refused"`, and with [`LoginOutcomeTally::unsuccessful`] **deliberately left unwired**
    /// (still the hand-written `self.failed + self.unknown`). That last point is not incidental: it
    /// is the single variable that decides which assertion the second red lands on, so the mutation
    /// script asserts the two-field body is still present verbatim and aborts if it is not. Call
    /// this **variant A**; **variant B** is the same mutation with `unsuccessful()` wired
    /// (`+ self.refused`). Both were run at round 9, at this head and at the round-7 head, so which
    /// run produced which number below is measured rather than inferred.
    ///
    /// ⚠️ **[EDIT, round 7 (#755 review, B2): "every other test in this module and in `eqoxide-http`
    /// stayed green; this one alone went RED" was false.]** ⚠️ **[EDIT, round 9 (#755 review, B1-r8):
    /// this block used to cite "the round-7 review's own re-run (`52 passed; 2 failed`)" as the proof
    /// of the two bullets below. That is a collapsed referent. Both runs print `52 passed; 2 failed`
    /// — 54 tests, two red either way — which hides the seam, but the round-7 review's second red was
    /// a **different assertion at a different line**; see the adjudication below.]** The two bullets
    /// are this round's own re-run of **variant A** at this head, `eqoxide-ipc`,
    /// `test result: FAILED. 52 passed; 2 failed`:
    ///
    /// - this test, exactly as designed — `left: 3, right: 4` distinct tokens
    ///   (`["failed", "succeeded", "failed", "unknown"]`). The claim this test itself makes is
    ///   correct.
    /// - [`no_interleaving_can_bury_a_login_failure_where_a_poller_cannot_find_it`] also goes RED, on
    ///   the very first verdict pair it tries (`[Refused, Refused]`): `left: 0, right: 2` at its
    ///   per-iteration [`LoginOutcomeTally::unsuccessful`] check. **This is not a second symptom of
    ///   the token collision** — it is the already-disclosed N3 residual on `unsuccessful()` (a
    ///   hand-written `failed + unknown` that cannot see a variant added outside those two fields),
    ///   reached here only because reproducing this mutation requires adding a real new outcome, and
    ///   that property test happens to assert `unsuccessful()` once per verdict pair. It fires before
    ///   the loop ever reaches that test's own `checked == 2520 * ALL.len().pow(2) * 8` assertion —
    ///   which used to be the literal `2520 * 9 * 8`, a same-class alphabet-size restatement, now
    ///   fixed on its own merits (see the note there) but not what caused *this* red.
    /// - No `eqoxide-http` test went red in this reproduction. Stated as scope, not as coverage:
    ///   round 9 ran `-p eqoxide-ipc` only, and a round-9 tree-wide grep puts every mention of
    ///   `unsuccessful` inside `crates/eqoxide-ipc/` — no caller in `eqoxide-http` or anywhere else
    ///   — so A and B differ there only in a function no other crate invokes.
    ///
    /// **The adjudication, all four cells measured at round 9** (`-p eqoxide-ipc --locked
    /// --no-fail-fast`; the round-7 head is reproduced exactly, since `5df7099..55ecbff` changes no
    /// file but this one, and its only non-comment edits are this test's name and the `checked`
    /// literal). What varies is *only* whether `unsuccessful()` is wired:
    ///
    /// | head | `unsuccessful()` | second red | `test result:` |
    /// |---|---|---|---|
    /// | round-7 `5df7099` | A, unwired | per-iteration `unsuccessful()` check, `0` vs `2` | `52 passed; 2 failed` |
    /// | round-7 `5df7099` | B, wired | the `checked` literal, **`322560` vs `181440`** | `52 passed; 2 failed` |
    /// | this head `55ecbff` | A, unwired | per-iteration `unsuccessful()` check, `0` vs `2` | `52 passed; 2 failed` |
    /// | this head `55ecbff` | B, wired | **none — that test is GREEN** | `53 passed; 1 failed` |
    ///
    /// So the round-7 review's `322560 vs 181440` was neither a mistake nor a different bug: it is
    /// row 2, the same mutation plus the one line that lets the loop run to completion. And row 4 is
    /// the direct measurement that deriving the literal from `ALL.len()` fixed a live defect — the
    /// only cell that changes across the two heads is the one where the loop actually reaches that
    /// assertion.
    ///
    /// Reverted afterward, `cp`-aside restored and `md5sum`-verified back to
    /// `13bac5cacbc6ec6b1d9fba8088ade684` — which is `git show 55ecbff:crates/eqoxide-ipc/src/asset_sync.rs`,
    /// the content the round-9 mutations were applied to, *not* this file at whatever HEAD you are
    /// reading it from (this very edit changes it).
    #[test]
    fn as_str_is_injective_over_all_so_no_two_outcomes_in_all_collide_on_one_wire_token() {
        let tokens: Vec<&str> = ConnectOutcome::ALL.iter().map(|o| o.as_str()).collect();
        let mut distinct = tokens.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(distinct.len(), tokens.len(),
            "as_str() must return a distinct token for every outcome in ALL — two outcomes sharing \
             one token collide in the encoder's token-keyed JSON map (last insert wins), which hides \
             a whole outcome category and mislabels the login that produced it: {tokens:?}");
    }

    /// **#743 review B3, as the rule rather than the pair of examples.** For every outcome, the
    /// counter and the retained record must agree, and a record must never appear under an outcome
    /// other than its own. This is what a shared slot could not satisfy at all.
    ///
    /// MUTATION-CHECK: make `LastLoginByOutcome::slot_mut` return `&mut self.failed` for
    /// `Unknown` — the round-2 collapse in miniature — and the `unknown` iteration goes RED.
    ///
    /// **Stated limit (#743 round-3 review B1, measured).** Despite the name, this test cannot see
    /// an outcome that was *added* to the enum and left out of [`ConnectOutcome::ALL`]: the `3`
    /// below is the number of logins this test stages, not `ALL.len()`, so a variant nothing
    /// constructs contributes zero to both sides. The review ran exactly that mutation and the whole
    /// suite stayed green (`223 passed; 0 failed` / `52 passed; 0 failed`). That direction is now a
    /// compile error in [`LastLoginByOutcome::slots`], not a failure here.
    ///
    /// The other direction — an outcome **removed** from `ALL` — is what this test genuinely used to
    /// kill, and its RED count was victim-dependent, which is worth recording because it was once
    /// quoted as if it were not: on the round-3 tree, dropping `Failed` killed 5 tests
    /// (`220 passed; 3 failed` / `50 passed; 2 failed`) and dropping `Unknown` killed 4
    /// (`221 passed; 2 failed` / `50 passed; 2 failed`). On *this* tree that mutation no longer
    /// reaches the suite at all: it fails to compile in `slots`. So the honest statement of this
    /// test's remaining value is that it pins the count/record biconditional per outcome, not that
    /// it guards `ALL`'s membership — the compiler does that in both directions now.
    #[test]
    fn every_outcome_is_in_all_so_no_category_can_go_unserved() {
        let s = new_shared();
        AssetConnectGuard::begin(&s, "zone load: qeynos2").finish(ConnectOutcome::Succeeded);
        AssetConnectGuard::begin(&s, "common asset load").finish(ConnectOutcome::Failed);
        drop(AssetConnectGuard::begin(&s, "model-sync worker (charmodel sets)"));
        // A set sync ends too, to prove it is counted in none of the login categories.
        drop(AssetSyncGuard::begin(&s, "zonedoors/neriakc"));

        let slots = lock(&s);
        let tally = slots.login_outcomes();
        assert_eq!(ConnectOutcome::ALL.iter().map(|o| tally.count(*o)).sum::<u64>(), 3,
            "three logins ended, so the per-outcome counts must sum to three — an outcome missing \
             from `ALL` shows up here as a login the endpoint would never serve a record for");
        for o in ConnectOutcome::ALL {
            assert_eq!(tally.count(o) > 0, slots.last_login().get(o).is_some(),
                "count and record must be present together for {o:?}: {tally:?}");
        }
        assert_eq!(slots.last_login().get(ConnectOutcome::Succeeded).map(|r| r.purpose.clone()),
            Some("zone load: qeynos2".into()));
        assert_eq!(slots.last_login().get(ConnectOutcome::Failed).map(|r| r.purpose.clone()),
            Some("common asset load".into()));
        assert_eq!(slots.last_login().get(ConnectOutcome::Unknown).map(|r| r.purpose.clone()),
            Some("model-sync worker (charmodel sets)".into()));
    }

    /// **#743 review N8, as behaviour instead of a comment.** Round 2 had one `end(id, outcome)` and
    /// the SET SYNC guard passing `ConnectOutcome::Unknown` as filler, defended by an argument that
    /// [`SyncId`]s cannot alias so the filler could never reach the login arm. The argument holds,
    /// but the reviewer's point stands: if it ever stopped holding, a set sync ending would
    /// **fabricate** a login failure — a monotonic `unknown` that can never be walked back, and a
    /// retained record naming a login that never ran, under another call's purpose.
    ///
    /// `end_sync` now takes no outcome, so there is nothing to fabricate FROM. This asserts the
    /// remaining half — what `end` does when handed the impossible input anyway. It is reachable
    /// only from inside this module, which is exactly why it can be tested here and could not be
    /// tested through the guards: the aliasing that would produce it in production cannot be staged.
    #[test]
    fn a_set_syncs_end_cannot_fabricate_a_login_observable_even_with_an_aliased_id() {
        let s = new_shared();
        let g = AssetConnectGuard::begin(&s, "common asset load");
        let login_id = g.id;
        std::mem::forget(g);            // keep the entry live; end it the wrong way, by hand

        lock(&s).end_sync(login_id);    // the aliased-id case, staged directly

        let slots = lock(&s);
        assert_eq!(slots.login_outcomes(), LoginOutcomeTally::default(),
            "no verdict was ever seen, so NOTHING may be counted — a fabricated `unknown` here is \
             monotonic and can never be corrected");
        for o in ConnectOutcome::ALL {
            assert!(slots.last_login().get(o).is_none(),
                "…and nothing may be retained under {o:?}: a record here names a login that, as far \
                 as this registry ever measured, did not end that way");
        }
        assert_eq!(slots.last_ended().map(|e| e.what.clone()),
            Some(EndedWhat::Connect {
                purpose: "common asset load".into(), outcome: ConnectOutcome::Unknown,
            }),
            "the entry did leave, so the TRANSIENT slot reports it — as `unknown`, which is what it \
             is: a login entry with no verdict ever seen. The asymmetry with the durable fields is \
             the point: last_ended is overwritten by the next activity, the counters never are");
        assert_eq!(slots.live().len(), 0, "and the entry must not be left behind as a phantom");
    }

    /// The shared-`Arc` identity trap, pinned at the type level (#743 review). A registry written
    /// through one clone MUST be readable through another — a severed write path is how this project
    /// has previously served a confident, silent, wrong answer.
    ///
    /// **What this does NOT cover, stated because the gap is the interesting part:** it pins the
    /// TYPE's sharing semantics, not that the four production login sites and `HttpState` were handed
    /// clones of the *same* registry. That is `main.rs`/`app.rs` wiring, unreachable from a unit
    /// test, and its only evidence is a live run in which one response body named all four purposes.
    #[test]
    fn a_clone_of_the_registry_is_the_same_registry_not_a_second_one() {
        let writer = new_shared();
        let reader = writer.clone();
        assert!(lock(&reader).live().len() == 0 && lock(&reader).last_ended().is_none());

        let g = AssetConnectGuard::begin(&writer, "zone load: neriakc");
        assert_eq!(live_all(&reader), ["connecting:zone load: neriakc"],
            "a login published through one handle must be visible through the other, or the \
             endpoint reads 'idle' forever no matter what the loaders publish");
        g.finish(ConnectOutcome::Failed);
        assert_eq!(lock(&reader).login_outcomes().failed, 1,
            "…and so must its verdict: {:?}", lock(&reader).login_outcomes());
    }

    /// **The universal, exhaustively.** "The endpoint never reports idle while the client is
    /// blocked" is a *never* claim, and no number of passing live runs discharges one. So this
    /// enumerates EVERY interleaving of three overlapping activities' begin/end — a login and two
    /// set syncs, 90 orderings — and after every single step asserts the registry is empty if and
    /// only if no guard is alive.
    ///
    /// That is the property `active` is derived from, and it covers both directions: an entry that
    /// outlives its guard (a finished sync reported as live) and a guard whose entry is missing
    /// (the #731 falsehood, and the #726 nested-clear one) each fail here regardless of ORDER,
    /// which is what a fixed example test cannot say.
    ///
    /// **Two alphabets (#743 review N1).** The original 1 login + 2 syncs (90 orderings) is kept
    /// verbatim, and a second alphabet of **2 logins + 2 syncs** (2520 orderings) is added, because
    /// three concurrent logins is the *normal* startup shape when the asset server is unreachable and
    /// the one-login alphabet never exercised two logins overlapping at all — the exact concurrency
    /// #743's B1 defect lives in. The larger alphabet is added, not substituted: it is a different
    /// enumeration, not a superset, and dropping the first would lose coverage rather than gain it.
    #[test]
    fn no_interleaving_of_logins_and_syncs_can_report_idle_while_one_is_alive() {
        /// Every multiset permutation of `[0,0,1,1,…,n-1,n-1]`; the first occurrence of `k` begins
        /// activity k, the second ends it. (2n)!/2^n sequences: 90 for n=3, 2520 for n=4.
        fn orderings(n: usize) -> Vec<Vec<usize>> {
            fn go(n: usize, cur: &mut Vec<usize>, used: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
                if cur.len() == 2 * n { out.push(cur.clone()); return; }
                for k in 0..n {
                    if used[k] < 2 {
                        used[k] += 1;
                        cur.push(k);
                        go(n, cur, used, out);
                        cur.pop();
                        used[k] -= 1;
                    }
                }
            }
            let mut out = Vec::new();
            go(n, &mut Vec::new(), &mut vec![0usize; n], &mut out);
            out
        }

        /// One live guard, either kind. `Option` so "ended" is `None` and dropping is explicit.
        enum Held { Sync(AssetSyncGuard), Connect(AssetConnectGuard) }

        // `logins` activities are LOGINS and the rest are set syncs. The mix is the point: the
        // registry must not care which kind an entry is, nor how many of each.
        for (n, logins, expected) in [(3usize, 1usize, 90usize), (4, 2, 2520)] {
            let all = orderings(n);
            assert_eq!(all.len(), expected,
                "the enumeration itself must be complete for {n} activities");
            for order in all {
                let s = new_shared();
                let mut held: Vec<Option<Held>> = (0..n).map(|_| None).collect();
                let mut alive = 0usize;
                for &k in &order {
                    match held[k].take() {
                        None => {
                            held[k] = Some(if k < logins {
                                Held::Connect(AssetConnectGuard::begin(
                                    &s, &format!("login{k}: zone load: qeynos2")))
                            } else {
                                Held::Sync(AssetSyncGuard::begin(&s, &format!("set{k}")))
                            });
                            alive += 1;
                        }
                        Some(g) => {
                            match g {
                                Held::Sync(g) => drop(g),
                                Held::Connect(g) => g.finish(ConnectOutcome::Succeeded),
                            }
                            alive -= 1;
                        }
                    }
                    let live = lock(&s).live().len();
                    assert_eq!(live, alive,
                        "after {order:?} step, {alive} activities are in flight but the registry \
                         lists {live} — an agent polling now would be told the client is doing \
                         {live} things while it is blocked in {alive}");
                    assert_eq!(live == 0, alive == 0,
                        "`active` is derived from emptiness, so these must agree exactly: {order:?}");
                }
                assert_eq!(lock(&s).live().len(), 0, "everything ended: {order:?}");
            }
        }
    }

    /// The same exhaustive treatment for the property #743's B1 turns on: **no interleaving of two
    /// logins and two syncs can make an ended login failure unobservable.** `last_ended` fails this
    /// for most orderings, which is why the assertion is against the retained fields.
    ///
    /// **Extended for #743 review round 3 (N9).** The first version drew login outcomes from
    /// `{Succeeded, Failed}` only, with exactly one failure per run — so the property it enumerated
    /// exhaustively was never the one B3 is about: two non-successes of DIFFERENT kinds, in both
    /// orders. Under round 2's single shared slot they destroyed each other, and no test in the
    /// alphabet could see it. The alphabet now spans **all 9 outcome pairs** over
    /// `{Succeeded, Failed, Unknown}` × 2520 orderings, and the model tracked alongside is the whole
    /// contract, re-asserted after every one of the 8 steps:
    ///
    /// - each of the three counters equals the number of logins that ended that way, and
    /// - for **each** outcome, `last_login().get(o)` is present **iff** that outcome's count is
    ///   non-zero, and
    /// - when present it is the **most recently ENDED login with that outcome** — never another
    ///   outcome's login, and never displaced by one.
    ///
    /// The per-outcome form is what makes B3 unrepeatable: any change that lets one outcome's login
    /// land in another's slot fails here in hundreds of orderings, rather than surviving because the
    /// only test that looked happened to use one kind.
    ///
    /// A login that ends WITHOUT `finish` is not a stand-in for the panic path — it *is* that path
    /// (`AssetConnectGuard::drop` with the untouched `Unknown`). The real `catch_unwind` panics live
    /// in `a_failure_and_a_panic_never_compete_for_a_slot_so_neither_can_hide_the_other`.
    #[test]
    fn no_interleaving_can_bury_a_login_failure_where_a_poller_cannot_find_it() {
        fn orderings(n: usize) -> Vec<Vec<usize>> {
            fn go(n: usize, cur: &mut Vec<usize>, used: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
                if cur.len() == 2 * n { out.push(cur.clone()); return; }
                for k in 0..n {
                    if used[k] < 2 {
                        used[k] += 1;
                        cur.push(k);
                        go(n, cur, used, out);
                        cur.pop();
                        used[k] -= 1;
                    }
                }
            }
            let mut out = Vec::new();
            go(n, &mut Vec::new(), &mut vec![0usize; n], &mut out);
            out
        }
        enum Held { Sync(AssetSyncGuard), Connect(AssetConnectGuard) }

        // Activities 0 and 1 are LOGINS; 2 and 3 are set syncs.
        const PURPOSE: [&str; 2] = ["common asset load", "zone load: neriakc"];
        // The model indexes by position in `ConnectOutcome::ALL`, so an outcome dropped from `ALL`
        // is a compile-time `position()` panic here rather than a silently unchecked category.
        let idx = |o: ConnectOutcome| ConnectOutcome::ALL.iter().position(|x| *x == o).unwrap();

        let mut checked = 0usize;
        for order in orderings(4) {
            for a in ConnectOutcome::ALL {
                for b in ConnectOutcome::ALL {
                    let verdict = [a, b];
                    let s = new_shared();
                    let mut held: Vec<Option<Held>> = (0..4).map(|_| None).collect();
                    // The contract, modelled independently of the code under test: a tally, and ONE
                    // most-recent purpose PER OUTCOME.
                    let mut tally = LoginOutcomeTally::default();
                    // ⚠️ [EDIT, round 6 (#755 review): this used to be a hard-coded `[Option<String>; 3]`
                    // — an un-enumerated restatement of the alphabet's size, in this property test's
                    // own model. An outcome added to `ALL` would have made `idx(o)` return 3+ and
                    // panicked here with "index out of bounds" before ever reaching this test's own
                    // `unsuccessful()` assertion, rather than failing informatively.]
                    let mut slots_model: Vec<Option<String>> = vec![None; ConnectOutcome::ALL.len()];
                    for &k in &order {
                        match held[k].take() {
                            None => held[k] = Some(match k {
                                0 | 1 => Held::Connect(AssetConnectGuard::begin(&s, PURPOSE[k])),
                                _ => Held::Sync(AssetSyncGuard::begin(&s, &format!("set{k}"))),
                            }),
                            Some(g) => match g {
                                Held::Sync(g) => drop(g),
                                Held::Connect(g) => {
                                    let o = verdict[k];
                                    // Dropping without `finish` IS the panic path, not a mock of it.
                                    if o == ConnectOutcome::Unknown { drop(g) } else { g.finish(o) }
                                    tally.record(o);
                                    slots_model[idx(o)] = Some(PURPOSE[k].into());
                                }
                            },
                        }
                        let slots = lock(&s);
                        assert_eq!(slots.login_outcomes(), tally,
                            "after {order:?} with verdicts {verdict:?}: the registry reports {:?} \
                             but {tally:?} is what happened — a caller polling here is told the \
                             wrong thing about logins that have already ended",
                            slots.login_outcomes());
                        for o in ConnectOutcome::ALL {
                            assert_eq!(slots.last_login().get(o).is_some(), tally.count(o) > 0,
                                "the record for {o:?} must be present exactly when its counter is \
                                 non-zero — one biconditional PER OUTCOME, which is what round 2's \
                                 shared slot could not offer: {order:?} {verdict:?}");
                            assert_eq!(slots.last_login().get(o).map(|r| r.purpose.clone()),
                                slots_model[idx(o)],
                                "the record for {o:?} must be the most recent login that ended \
                                 {o:?} — not another outcome's login, and not displaced by one: \
                                 {order:?} {verdict:?}");
                        }
                        drop(slots);
                        checked += 1;
                    }
                    // Ending state: every login has ended, so the tally is the whole verdict set.
                    let slots = lock(&s);
                    assert_eq!(slots.login_outcomes().unsuccessful(),
                        verdict.iter().filter(|o| **o != ConnectOutcome::Succeeded).count() as u64,
                        "…and at the end, for {order:?} {verdict:?}");
                    assert_eq!(slots.live().len(), 0);
                }
            }
        }
        // ⚠️ [EDIT, round 7 (#755 review): this used to be the literal `2520 * 9 * 8` — 9 = 3², the
        // same un-enumerated alphabet-size restatement `slots_model` had twelve lines above, in the
        // same test. The review named it as a live bug: add a variant and `ALL.len()` grows from 3
        // to 4, so the *actual* pair count the two nested `for … in ConnectOutcome::ALL` loops walk
        // grows to 16 while the hard-coded `9` stays put, so `checked` (322560) and the literal
        // (181440) diverge.
        //
        // ⚠️ [EDIT, round 9 (#755 review, B1-r8): the clause that stood here said "in the round-7
        // MX-E2 re-run this line was never reached — an earlier, unrelated, already-disclosed gap
        // fired first". That is false about the run it names, and self-contradicting on its face:
        // the two numbers quoted one line above can only be produced by a run that DID reach this
        // line. Corrected, and re-measured at round 9 rather than reasoned about — all four cells of
        // the 2×2 are in the MUTATION-CHECK table on
        // `as_str_is_injective_over_all_so_no_two_outcomes_in_all_collide_on_one_wire_token`:
        //
        //   * the round-7 review's mutation ALSO wired `LoginOutcomeTally::unsuccessful()`, so the
        //     per-iteration `unsuccessful()` check above passed, the loop ran to completion, and this
        //     line WAS reached and DID fire `322560 vs 181440` at the round-7 head. That is the
        //     direct measurement that this literal was a live bug, not merely a derivation from
        //     `ALL.len()` — which makes the case for the fix stronger, not weaker.
        //   * the mutation described in that MUTATION-CHECK leaves `unsuccessful()` unwired, so the
        //     per-iteration check panics on the first verdict pair and this line is not reached.
        //
        // Whether it is reached is therefore a property of the mutation, not of this defect: the
        // divergence is real either way. Derived from `ALL.len()` so the count self-scales with the
        // alphabet the loops actually walk, regardless of which assertion a given mutation trips
        // first — and at this head, with `unsuccessful()` wired, that test is GREEN under the same
        // mutation that reddened it at the round-7 head.]
        assert_eq!(checked, 2520 * ConnectOutcome::ALL.len().pow(2) * 8,
            "2520 orderings × ALL.len()² outcome pairs × 8 steps must each have been asserted; a \
             shrunken enumeration is how this property stops covering the ordering it exists for");
    }

    /// A login and the syncs it enables share the registry, keep the oldest-first order, and own
    /// their entries independently — the same ownership property #726 established for syncs.
    #[test]
    fn a_login_and_a_sync_coexist_and_neither_erases_the_other() {
        let s = new_shared();
        let zone = AssetSyncGuard::begin(&s, "zone/neriakc");
        zone.tick(dl(120));
        let login = AssetConnectGuard::begin(&s, "model-sync worker (charmodel sets)");
        assert_eq!(live_all(&s),
            ["zone/neriakc", "connecting:model-sync worker (charmodel sets)"]);
        login.finish(ConnectOutcome::Succeeded);
        assert_eq!(live_all(&s), ["zone/neriakc"],
            "a login finishing must not erase a zone download that is still in flight");
        assert_eq!(phase_of(&s, "zone/neriakc"), Some(dl(120)), "…nor reset its progress");
    }

    // ── the rate's honesty rules ────────────────────────────────────────────────────────────────

    #[test]
    fn a_rate_is_withheld_while_the_phase_is_too_young_to_divide() {
        assert_eq!(
            observed_download_rate(1_048_576, Duration::from_millis(50), Duration::ZERO),
            Err("phase_too_young"));
    }

    /// #726 review finding 2, with the reviewer's own measured numbers. A wedged transfer keeps a
    /// perfectly plausible rate frozen in place; the client knows the sample is minutes old and must
    /// stop asserting the number rather than serve it beside an age and hope the caller checks.
    #[test]
    fn a_rate_is_withheld_once_its_sample_is_too_stale_to_still_be_true() {
        let (bytes, elapsed) = (31_294_024u64, Duration::from_secs_f64(20.65));
        // Fresh: the number is real and is published.
        let fresh = observed_download_rate(bytes, elapsed, Duration::from_millis(41))
            .expect("a fresh sample carries its rate");
        assert!((fresh - 1_515_449.0).abs() < 1_000.0, "1.5 MB/s, got {fresh}");
        // Still published across the measured healthy cadence, including its worst observed tail.
        assert!(observed_download_rate(bytes, elapsed, Duration::from_millis(469)).is_ok(),
            "the slowest healthy inter-tick gap measured live (469 ms) must NOT suppress the rate — \
             a threshold that fires during a healthy load is its own kind of false alarm");
        // Wedged five minutes: the true phase average is ~97,600 B/s and the instantaneous rate is
        // zero, so 1,515,404 is not a stale truth, it is a falsehood.
        assert_eq!(observed_download_rate(bytes, elapsed, Duration::from_secs(300)),
            Err("sample_too_stale"));
    }

    /// #726 review round 2 — the documented precedence between the two withholding rules. When a
    /// sample is BOTH under the 100 ms minimum elapsed and older than the staleness bound, the
    /// reasons are not interchangeable: `phase_too_young` reads as "a rate is coming, wait", and
    /// telling an agent to keep waiting on a transfer that has moved nothing for five minutes is the
    /// more harmful of the two answers. The reviewer's mutation M-R6 reordered the checks and the
    /// whole suite stayed green; nothing pinned a rule the docs state outright.
    ///
    /// The case is reachable, not hypothetical: a download enters `Downloading`, publishes its first
    /// sample before 100 ms of phase elapsed, and then wedges.
    #[test]
    fn staleness_outranks_youth_when_a_sample_is_both() {
        assert_eq!(
            observed_download_rate(4_096, Duration::from_millis(50), Duration::from_secs(300)),
            Err("sample_too_stale"),
            "a sample that is both too young to divide AND five minutes old must report the \
             staleness — 'wait, a rate is coming' is the wrong thing to tell an agent about a \
             transfer that has stopped");
    }

    #[test]
    fn the_rate_threshold_is_the_documented_one() {
        // The number is quoted in docs/http-api.md and in the response's own `semantics` string, so
        // it must not drift from either without this failing.
        assert_eq!(MAX_RATE_SAMPLE_AGE, Duration::from_millis(2000));
        assert!(observed_download_rate(1_000_000, Duration::from_secs(1),
            MAX_RATE_SAMPLE_AGE).is_ok(), "exactly at the bound is still published");
        assert_eq!(observed_download_rate(1_000_000, Duration::from_secs(1),
            MAX_RATE_SAMPLE_AGE + Duration::from_millis(1)), Err("sample_too_stale"));
        // #726 review round 2 observed that widening the constant is caught SOLELY by the assert_eq
        // above, because every other assertion in this test is expressed relative to
        // MAX_RATE_SAMPLE_AGE and moves with it — a test that pins a value while blind to whether
        // anything acts on it. These two bounds are ABSOLUTE, so they fail on a widened or narrowed
        // threshold even if the literal above were updated to match. 400 ms is inside the measured
        // healthy cadence (worst observed gap 491 ms); 2500 ms is outside any cadence measured.
        assert!(observed_download_rate(1_000_000, Duration::from_secs(1),
            Duration::from_millis(400)).is_ok(),
            "narrowing the threshold under the measured healthy cadence would withhold rates \
             during a perfectly healthy load");
        assert_eq!(observed_download_rate(1_000_000, Duration::from_secs(1),
            Duration::from_millis(2500)), Err("sample_too_stale"),
            "widening the threshold past ~2 s would republish rates the client has no current \
             evidence for");
    }
}
