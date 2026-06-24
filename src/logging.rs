//! Process-wide `tracing` initialization.
//!
//! Replaces the old ad-hoc `eprintln!`/`println!` logging. Every diagnostic in the client now goes
//! through `tracing` macros (`error!`/`warn!`/`info!`/`debug!`/`trace!`), so verbosity is controlled
//! at runtime by an env filter instead of being hard-compiled.
//!
//! Filter precedence: `EQ_LOG`, then `RUST_LOG`, then a default of `info` (which preserves the old
//! behaviour where every `eprintln!` was visible). Examples:
//!
//! ```text
//! EQ_LOG=debug                 # everything at debug and above
//! EQ_LOG=info,eq_renderer::eq_net=debug   # net subsystem chattier than the rest
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
        let filter = EnvFilter::try_from_env("EQ_LOG")
            .or_else(|_| EnvFilter::try_from_default_env()) // RUST_LOG
            .unwrap_or_else(|_| EnvFilter::new("info"));

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
