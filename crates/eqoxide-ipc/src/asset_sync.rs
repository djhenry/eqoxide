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
//! ## Who writes it, and when an entry is REMOVED
//!
//! The sole writer is [`AssetSyncGuard`], created by the app crate's
//! `asset_sync::sync_set_observed` around every `sync_set` call — which is the *only* way to reach
//! `sync_set` at all (it is private to that module), so an unobserved in-session sync is
//! unrepresentable rather than merely discouraged. The guard removes its own entry in `Drop`, so
//! the "published but never cleared" failure (an endpoint confidently reporting a long-finished
//! sync as if it were live) cannot happen on ANY exit path — success, error return, or a panic
//! unwinding through the loader thread.

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
    last_ended: Option<EndedSync>,
}

impl AssetSyncSlots {
    /// Every live sync, oldest-started first.
    pub fn live(&self) -> impl ExactSizeIterator<Item = &AssetSyncActivity> {
        self.live.iter().map(|(_, a)| a)
    }

    /// The most recent sync to leave the registry, or `None` if none ever has.
    pub fn last_ended(&self) -> Option<&EndedSync> {
        self.last_ended.as_ref()
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
    fn tick(&mut self, id: SyncId, phase: AssetSyncPhase, published_at: Instant) {
        if let Some((_, a)) = self.live.iter_mut().find(|(i, _)| *i == id) {
            a.phase = phase;
            a.published_at = published_at;
        }
    }

    /// Remove `id`'s entry — and ONLY `id`'s. `Vec::remove` preserves the relative order of the
    /// rest, so the oldest-first invariant survives a middle entry ending.
    fn end(&mut self, id: SyncId) {
        if let Some(pos) = self.live.iter().position(|(i, _)| *i == id) {
            let (_, a) = self.live.remove(pos);
            self.last_ended = Some(EndedSync { set: a.set, at: Instant::now() });
        }
    }
}

/// A sync that has left the registry. **`ended` means ended, not succeeded** — the guard's `Drop`
/// runs identically on the success return, the error return, and a panic unwind, and it genuinely
/// cannot tell them apart. This exists only so an agent can distinguish "a sync finished a moment
/// ago" from "no sync has ever run in this process", which were previously the same empty answer
/// (the known-empty vs unknown collapse, #726 review N5).
#[derive(Clone, Debug, PartialEq)]
pub struct EndedSync {
    pub set: String,
    /// When it ended. An `Instant`, turned into an age at READ time — never a cached age (#343).
    pub at: Instant,
}

/// A read-time copy of the whole registry, so the HTTP handler holds no lock while serializing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct AssetSyncSnapshot {
    /// Oldest-started first — see [`AssetSyncSlots`].
    pub live: Vec<AssetSyncActivity>,
    pub last_ended: Option<EndedSync>,
}

/// Copy the registry for a reader. The clone is deliberate: every age in the response is measured
/// from an `Instant` *after* the lock is released, and holding the loaders' mutex across
/// serialization would let a slow reader stall a download tick.
pub fn snapshot(shared: &AssetSyncShared) -> AssetSyncSnapshot {
    let slots = lock(shared);
    AssetSyncSnapshot {
        live: slots.live().cloned().collect(),
        last_ended: slots.last_ended().cloned(),
    }
}

/// One in-flight `sync_set` call, as an observer sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetSyncActivity {
    /// The asset-server set being synced, verbatim — e.g. `"zone/qeynos2"`, `"zonedoors/qeynos2"`,
    /// `"common"`, `"charmodel/hum"`. This is what tells an observer WHICH sync a sample describes
    /// when loaders overlap. It is NOT an identity: two syncs of the same set can be live at once,
    /// which is why ownership is by [`SyncId`].
    pub set: String,
    pub phase: AssetSyncPhase,
    /// When the `sync_set` call began. Turned into a live "has been running for N ms" at read time.
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
    pub published_at: Instant,
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
            set: set.to_string(),
            phase: AssetSyncPhase::Starting,
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
        lock(&self.shared).end(self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_sets(s: &AssetSyncShared) -> Vec<String> {
        lock(s).live().map(|a| a.set.clone()).collect()
    }

    fn dl(chunks_done: usize) -> AssetSyncPhase {
        AssetSyncPhase::Downloading {
            chunks_done, chunks_total: 374, bytes: 31_294_024, elapsed: Duration::from_secs(20),
        }
    }

    fn phase_of(s: &AssetSyncShared, set: &str) -> Option<AssetSyncPhase> {
        lock(s).live().find(|a| a.set == set).map(|a| a.phase.clone())
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
        assert_eq!(a.set, "common");
        assert_eq!(a.phase, AssetSyncPhase::Starting);
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
        assert_eq!(ended.set, "zone/neriakc");
    }

    #[test]
    fn a_ticks_phase_cannot_resurrect_a_sync_that_already_ended() {
        // Defensive: `tick` finds nothing for a removed id and must stay a no-op rather than
        // re-inserting a phantom entry that no guard owns and nothing will ever clear.
        let s = new_shared();
        let g = AssetSyncGuard::begin(&s, "common");
        lock(&s).end(g.id);
        g.tick(dl(1));
        assert!(live_sets(&s).is_empty(), "a tick after the end must not republish the sync");
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
