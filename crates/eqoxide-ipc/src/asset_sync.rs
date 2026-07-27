//! The agent-observable **asset-sync activity** slot (#715).
//!
//! ## Why this exists
//!
//! Zone/common asset sync progress (phase, chunks done/total, bytes, download rate) was rendered
//! only on the loading-screen HUD. This client is driven by an AI agent that has no eyes on the
//! screen: during a zone load it could observe *that* the client was not ready (via
//! [`crate::…`]-adjacent `zone_assets`, #579) but nothing about whether the load was progressing,
//! stalled, nearly done, or wedged. This slot is that missing channel, served on
//! `GET /v1/observe/asset_sync`.
//!
//! ## Why an enum, and why an `Option` around it
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
//! 2. **"No sync in progress" is `None`, not a zeroed struct.** An idle client and a download
//!    stalled at 0/N are different situations an agent acts on differently. `Option<…>` makes them
//!    different values rather than the same zeroes.
//!
//! ## Who writes it, and when it is CLEARED
//!
//! The sole writer is [`AssetSyncGuard`], created by the app crate's
//! `asset_sync::sync_set_observed` around every `sync_set` call. The guard clears the slot in
//! `Drop`, so the "published but never cleared" failure (an endpoint confidently reporting a long-
//! finished sync as if it were live) cannot happen on ANY exit path — success, error return, or a
//! panic unwinding through the loader thread. See [`AssetSyncGuard`] for the overlapping-syncs rule.

use std::sync::{Arc, Mutex};

/// Shared handle to the current asset-sync activity. `None` = **no sync is in progress**; that is a
/// different state from a sync sitting at zero progress, which is `Some` with a zeroed
/// [`AssetSyncPhase::Downloading`].
///
/// Constructed ONCE in `main.rs` and cloned — by `Arc` identity — into both the app (the writer)
/// and `HttpState` (the reader). Constructing a second, independent cell on either side would
/// silently sever the two and the endpoint would read "idle" forever no matter what the loaders
/// published (the shared-`Arc` trap the #616 review caught for `common_assets_failed`).
pub type AssetSyncShared = Arc<Mutex<Option<AssetSyncActivity>>>;

/// One in-flight `sync_set` call, as an observer sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetSyncActivity {
    /// The asset-server set being synced, verbatim — e.g. `"zone/qeynos2"`, `"zonedoors/qeynos2"`,
    /// `"common"`, `"charmodel/hum"`. This is what tells an observer WHICH sync a sample describes
    /// when two loaders overlap (a zone change while the previous zone's loader is still running).
    pub set: String,
    pub phase: AssetSyncPhase,
    /// When this sample was published. **The staleness answer, and the reason it is a timestamp
    /// rather than an age.**
    ///
    /// Every field beside it is a snapshot from the moment the producer last ticked, and the
    /// producer ticks only when a chunk completes. A download that WEDGES mid-chunk — a hung
    /// socket, an asset server that stopped answering — therefore leaves `chunks_done`, `bytes`
    /// and `elapsed` frozen at their last values with nobody left to update them, and a rate
    /// divided out of them would keep asserting a confident "1.2 MB/s" for a transfer that has
    /// moved zero bytes in five minutes. That is the shape of #343 (`connected: true` published
    /// only by a loop that had stopped running) and its repeat in #679: an observable with no live
    /// writer, still reading healthy.
    ///
    /// A guard cannot fix that by clearing, because a wedged sync genuinely IS still in progress —
    /// "no sync running" would be a worse answer than a stale one. What makes the stale answer
    /// honest is publishing how old it is, so the reader can tell "progressing" from "frozen"
    /// without having to diff two polls. Stored as an `Instant` and turned into an age at READ
    /// time; an age computed at WRITE time is itself a value that goes stale, which is the same bug
    /// one level down.
    pub published_at: std::time::Instant,
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
    /// slot `None` until the first producer tick would report "no sync in progress" while one
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
        /// chunk yet), distinct from `None` activity (no sync at all).
        chunks_done: usize,
        chunks_total: usize,
        /// Cumulative bytes transferred in this downloading phase.
        bytes: u64,
        /// Time since the START of the current downloading phase only (see the producer's
        /// `fetch_and_reassemble`), so a rate derived from it is a phase-local session average and
        /// cannot be inflated by time spent verifying beforehand.
        elapsed: std::time::Duration,
    },
}

/// Minimum elapsed time before a `Downloading` tick's `bytes`/`elapsed` are considered stable enough
/// to divide into a rate. The very first tick of a phase can land well under a millisecond after
/// `Instant::now()` was captured (a tiny/local chunk, warm caches) — dividing by that would produce
/// an absurd, noisy spike rather than an honest "not enough data yet" (#708 requirement 4).
const MIN_RATE_ELAPSED: std::time::Duration = std::time::Duration::from_millis(100);

/// Pure function: derives a bytes/sec rate from cumulative bytes transferred and elapsed time, or
/// `None` if `elapsed` is too small to divide by safely (see `MIN_RATE_ELAPSED`). Deliberately takes
/// plain `(u64, Duration)` rather than a `SyncProgress` so it's trivial to unit-test every edge
/// (zero elapsed, near-zero elapsed, zero bytes) without constructing a whole sync pipeline.
///
/// Defined here rather than in the app crate's `asset_sync` (which re-exports it verbatim, so every
/// #708 call site is unchanged) because BOTH the loading-screen HUD and `GET /v1/observe/asset_sync`
/// must derive the rate the same way — and `eqoxide-http` cannot reach up into the app crate. One
/// definition, one 100 ms threshold, two readers (#715).
pub fn download_rate_bytes_per_sec(bytes: u64, elapsed: std::time::Duration) -> Option<f64> {
    if elapsed < MIN_RATE_ELAPSED {
        return None;
    }
    Some(bytes as f64 / elapsed.as_secs_f64())
}

/// Publishes `phase` for `set` as the current activity, replacing whatever was there.
///
/// Last-writer-wins across overlapping syncs. Every sample is still individually true — the `set`
/// field says which sync it describes — but an observer polling during an overlap may see the two
/// interleave. See [`AssetSyncGuard`].
pub fn publish(shared: &AssetSyncShared, set: &str, phase: AssetSyncPhase) {
    let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
    // Stamped on EVERY publish, including a repeat of the same phase: the stamp's job is to say
    // when this sample was last known good, so a phase that stops being republished must start
    // ageing. See `AssetSyncActivity::published_at`.
    *slot = Some(AssetSyncActivity {
        set: set.to_string(),
        phase,
        published_at: std::time::Instant::now(),
    });
}

/// Clears the activity — but ONLY if what is published is still `set`'s own.
///
/// The guard rule that keeps a finishing loader from erasing a newer one: on a zone change the
/// previous zone's loader is still running and will finish (or fail) at some arbitrary later point.
/// An unconditional clear there would blank out the CURRENT zone's live progress and report "no
/// sync in progress" while the zone the agent is waiting on is still downloading.
pub fn finish(shared: &AssetSyncShared, set: &str) {
    let mut slot = shared.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|a| a.set == set) {
        *slot = None;
    }
}

/// RAII writer for [`AssetSyncShared`]: publishes [`AssetSyncPhase::Starting`] on construction,
/// [`Self::tick`] for each producer phase, and clears in `Drop`.
///
/// `Drop` — not an explicit call at the end of the happy path — is the point. The recurring defect
/// class here is an observable that is written but never cleared, which turns the endpoint into a
/// confident report of a sync that finished long ago. A guard clears on the success return, on the
/// error return, and on a panic unwinding out of the loader thread, with no code path left to
/// forget.
pub struct AssetSyncGuard {
    shared: AssetSyncShared,
    set: String,
}

impl AssetSyncGuard {
    /// Starts observing a sync of `set`, publishing [`AssetSyncPhase::Starting`] immediately.
    pub fn begin(shared: &AssetSyncShared, set: &str) -> Self {
        publish(shared, set, AssetSyncPhase::Starting);
        Self { shared: shared.clone(), set: set.to_string() }
    }

    /// Publishes a producer phase for this guard's set.
    pub fn tick(&self, phase: AssetSyncPhase) {
        publish(&self.shared, &self.set, phase);
    }
}

impl Drop for AssetSyncGuard {
    fn drop(&mut self) {
        finish(&self.shared, &self.set);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot() -> AssetSyncShared {
        Arc::new(Mutex::new(None))
    }

    fn read(s: &AssetSyncShared) -> Option<AssetSyncActivity> {
        s.lock().unwrap().clone()
    }

    #[test]
    fn guard_clears_the_slot_when_it_drops() {
        // The "published but never cleared" trap: an endpoint that keeps reporting a finished sync
        // as if it were live. Dropping the guard is the ONLY completion signal, so it must clear.
        let s = slot();
        {
            let g = AssetSyncGuard::begin(&s, "zone/qeynos2");
            g.tick(AssetSyncPhase::Downloading {
                chunks_done: 3, chunks_total: 7, bytes: 1024, elapsed: std::time::Duration::from_secs(1),
            });
            assert!(read(&s).is_some(), "a live sync must be observable while it runs");
        }
        assert_eq!(read(&s), None,
            "the slot must read 'no sync in progress' once the sync is over — a stale completed \
             sync reported as live is worse than reporting nothing");
    }

    #[test]
    fn guard_clears_even_when_the_thread_panics() {
        // A loader thread panicking is one of the paths #595/#616 already had to backstop. An
        // explicit end-of-happy-path clear would be skipped by the unwind and freeze the endpoint
        // on the phase the sync died in, forever.
        let s = slot();
        let s2 = s.clone();
        let r = std::panic::catch_unwind(move || {
            let _g = AssetSyncGuard::begin(&s2, "zone/qeynos2");
            panic!("loader thread died mid-sync");
        });
        assert!(r.is_err(), "the test must actually have panicked");
        assert_eq!(read(&s), None, "an unwind must still leave the slot honest");
    }

    #[test]
    fn a_finishing_sync_does_not_erase_a_different_sync_that_is_still_running() {
        // Zone change: the previous zone's loader is still alive and finishes at some arbitrary
        // later point. Clearing unconditionally there would blank out the CURRENT zone's live
        // progress and report "no sync in progress" while the agent is waiting on that very load.
        let s = slot();
        let old = AssetSyncGuard::begin(&s, "zone/qeynos2");
        let _new = AssetSyncGuard::begin(&s, "zone/freportw");
        drop(old);
        assert_eq!(
            read(&s).map(|a| a.set).as_deref(),
            Some("zone/freportw"),
            "an older loader finishing must not erase the newer sync that is still in flight"
        );
    }

    #[test]
    fn begin_publishes_starting_so_the_pre_tick_window_is_not_reported_as_idle() {
        // Between the sync_set call starting and its first producer tick the manifest request is in
        // flight. Leaving the slot `None` there would report "no sync in progress" while one was.
        let s = slot();
        let _g = AssetSyncGuard::begin(&s, "common");
        let a = read(&s).expect("a sync that has begun must be observable before its first tick");
        assert_eq!(a.set, "common");
        assert_eq!(a.phase, AssetSyncPhase::Starting);
    }
}
