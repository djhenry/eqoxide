//! Lightweight, zero-dependency per-phase frame profiling.
//!
//! Enabled with the `--profile` CLI flag (or `EQ_PROFILE=1`). When on, `app::render_frame` times each
//! phase of the frame (update / 3D render / egui / submit) with plain `Instant`s and stores the result
//! in a smoothed [`FrameProfile`], which the HUD draws as an overlay. There are no heavyweight profiler
//! deps (Tracy/puffin) — just `std::time` — so it is always compiled in and costs nothing when off.
//!
//! For finer-grained inspection, the timed phases also open `tracing` spans, so anyone who wants a full
//! timeline can attach a span-timing `tracing` layer without further code changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

/// Turn frame profiling on/off (set once at startup from the `--profile` flag / `EQ_PROFILE` env).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Whether the `--profile` overlay/timing is active.
#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Smoothed per-phase timings (milliseconds) for the HUD overlay. All zero until the first profiled
/// frame. Each field is an exponential moving average so the on-screen numbers are readable rather
/// than flickering frame-to-frame.
#[derive(Default, Clone, Copy)]
pub struct FrameProfile {
    pub update_ms: f32,
    pub render_ms: f32,
    pub egui_ms:   f32,
    pub submit_ms: f32,
    pub total_ms:  f32,
    /// Instantaneous frames-per-second derived from `total` + idle wait (wall-clock between frames).
    pub frame_ms:  f32,
}

impl FrameProfile {
    /// Blend a fresh per-frame sample into the running average.
    pub fn blend(&mut self, s: &FrameSample, frame_ms: f32) {
        const A: f32 = 0.12; // EMA weight — ~0.5s settling at 60fps
        self.update_ms += (s.update_ms() - self.update_ms) * A;
        self.render_ms += (s.render_ms() - self.render_ms) * A;
        self.egui_ms   += (s.egui_ms()   - self.egui_ms)   * A;
        self.submit_ms += (s.submit_ms() - self.submit_ms) * A;
        self.total_ms  += (s.total_ms()  - self.total_ms)  * A;
        self.frame_ms  += (frame_ms      - self.frame_ms)  * A;
    }
}

/// Raw per-phase durations captured during one `render_frame`. Built only when [`enabled`].
#[derive(Default)]
pub struct FrameSample {
    pub update: std::time::Duration,
    pub render: std::time::Duration,
    pub egui:   std::time::Duration,
    pub submit: std::time::Duration,
    pub total:  std::time::Duration,
}

impl FrameSample {
    pub fn update_ms(&self) -> f32 { self.update.as_secs_f32() * 1000.0 }
    pub fn render_ms(&self) -> f32 { self.render.as_secs_f32() * 1000.0 }
    pub fn egui_ms(&self)   -> f32 { self.egui.as_secs_f32()   * 1000.0 }
    pub fn submit_ms(&self) -> f32 { self.submit.as_secs_f32() * 1000.0 }
    pub fn total_ms(&self)  -> f32 { self.total.as_secs_f32()  * 1000.0 }
}

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
