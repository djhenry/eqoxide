//! Process-wide `tracing` initialization.
//!
//! Replaces the old ad-hoc `eprintln!`/`println!` logging. Every diagnostic in the client now goes
//! through `tracing` macros (`error!`/`warn!`/`info!`/`debug!`/`trace!`), so verbosity is controlled
//! at runtime by an env filter instead of being hard-compiled.
//!
//! Filter precedence: `EQ_LOG`, then `RUST_LOG`, then a default of
//! `info,wgpu_core=warn,wgpu_hal=warn,wgpu=warn,naga=warn` — our own diagnostics at `info`, but the
//! graphics stack pinned to `warn` so wgpu's per-frame "waiting for submission index" INFO spam
//! doesn't flood the console (real wgpu warnings/errors still show). Examples:
//!
//! ```text
//! EQ_LOG=debug                 # everything at debug and above
//! EQ_LOG=info,eqoxide::eq_net=debug   # net subsystem chattier than the rest
//! EQ_LOG=warn                  # quiet: only warnings and errors
//! ```
//!
//! Output goes to stderr (same stream the old `eprintln!` used) so existing run scripts that capture
//! stderr keep working.

use std::sync::Once;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

static INIT: Once = Once::new();

/// Install the global tracing subscriber. Idempotent — safe to call from multiple binaries/tests;
/// only the first call takes effect.
pub fn init() {
    INIT.call_once(|| {
        // Default (no EQ_LOG/RUST_LOG set): our own `info`, but quiet the graphics stack. wgpu-core
        // logs "Device::maintain: waiting for submission index …" at INFO on every
        // device.poll(Maintain::Wait) (once per frame), which floods the console; naga/wgpu_hal are
        // similarly chatty at info. Pin those targets to `warn` so real wgpu warnings/errors (e.g.
        // unsupported present modes) still surface. Set EQ_LOG/RUST_LOG to override entirely.
        const DEFAULT_FILTER: &str = "info,wgpu_core=warn,wgpu_hal=warn,wgpu=warn,naga=warn";
        let filter = EnvFilter::try_from_env("EQ_LOG")
            .or_else(|_| EnvFilter::try_from_default_env()) // RUST_LOG
            .unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

        let fmt_layer = fmt::layer()
            .with_target(true)
            .with_writer(std::io::stderr)
            .compact();

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
    });
}
