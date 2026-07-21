//! Lightweight, zero-dependency per-phase frame profiling.
//!
//! Enabled with the `--profile` CLI flag (or `EQ_PROFILE=1`). When on, `app::render_frame` times each
//! phase of the frame (update / 3D render / egui / submit) with plain `Instant`s and stores the result
//! in a smoothed [`FrameProfile`], which the HUD draws as an overlay. There are no heavyweight profiler
//! deps (Tracy/puffin) — just `std::time` — so it is always compiled in and costs nothing when off.
//!
//! For finer-grained inspection, the timed phases also open `tracing` spans, so anyone who wants a full
//! timeline can attach a span-timing `tracing` layer without further code changes.

use std::time::Instant;

// `FrameProfile` (the smoothed HUD/JSON timings) and `FrameSample` (a raw per-frame capture) are
// pure inter-thread contract data — they moved DOWN into `eqoxide-ipc` (#544 Step 2c) so that
// crate's `FrameProfileShared` slot no longer up-references `profiling`. `FrameProfile::blend` moved
// with them (an inherent impl must live with its type). The on/off toggle (`enabled`/`set_enabled`)
// moved down too (#544 Step 2o), so `eqoxide-ui` can gate its per-window timing log without an
// up-reference into this app crate. The `Stopwatch` collection helper below is the only thing that
// stays here. Re-exported so every existing `crate::profiling::{FrameProfile, FrameSample, enabled,
// set_enabled}` path across the tree keeps resolving unchanged.
pub use eqoxide_ipc::{enabled, set_enabled, FrameProfile, FrameSample};

// `FrameProfile` (+ its `blend` impl), `FrameSample` (+ its accessors), and the `enabled`/
// `set_enabled` toggle moved to `eqoxide-ipc` (#544 Steps 2c/2o) — re-exported at the top of this
// module.

/// Tiny RAII-free stopwatch. `let t = Stopwatch::start();` … `t.elapsed()`.
pub struct Stopwatch(Instant);

impl Stopwatch {
    #[inline]
    pub fn start() -> Self {
        Stopwatch(Instant::now())
    }
    #[inline]
    pub fn elapsed(&self) -> std::time::Duration {
        self.0.elapsed()
    }
}
