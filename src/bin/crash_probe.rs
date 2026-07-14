//! Test probe for the #380 crash-observability module. Not a user-facing tool.
//!
//! `tests/crash_signals.rs` runs this binary as a subprocess and inspects what it leaves behind.
//! It exists because the properties that matter here are *process-level* — "does the binary start",
//! "does a stack overflow still print std's loud message", "does a segfault leave a record" — and
//! none of them can be observed from inside a `cargo test --lib` process that must survive to report
//! its own results.
//!
//! That gap is exactly how the first version of this fix shipped a client that could not start at
//! all: `cargo test --lib` was green, and nothing ever executed the install path.
//!
//! Modes (argv[1]):
//!   startup              — install, print a marker, exit 0. Proves the process can boot.
//!   panic-worker         — panic on a named non-main thread, then exit 0 (the process survives).
//!   segv                 — dereference a null pointer on the main thread.
//!   stack-overflow       — infinite recursion on a named worker thread.
//!   abort                — std::process::abort().

use std::hint::black_box;

fn main() {
    eqoxide::crash::install();

    let mode = std::env::args().nth(1).unwrap_or_default();
    match mode.as_str() {
        "startup" => {
            println!("PROBE STARTED OK");
        }
        "panic-worker" => {
            let h = std::thread::Builder::new()
                .name("probe-worker".into())
                .spawn(|| panic!("probe worker panic"))
                .expect("spawn probe worker");
            let _ = h.join();
            println!("PROBE SURVIVED WORKER PANIC");
        }
        "segv" => {
            // A deliberate wild write, standing in for the GPU-driver faults in this binary's real
            // coredumpctl history.
            unsafe {
                let p: *mut u8 = std::ptr::null_mut();
                std::ptr::write_volatile(black_box(p), 1);
            }
        }
        "stack-overflow" => {
            let h = std::thread::Builder::new()
                .name("probe-overflow".into())
                .spawn(|| {
                    recurse(0);
                })
                .expect("spawn probe overflow thread");
            let _ = h.join();
        }
        "abort" => {
            std::process::abort();
        }
        other => {
            eprintln!("crash_probe: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

/// Unbounded recursion with a live stack frame the optimizer can't fold away.
#[allow(unconditional_recursion)] // the entire point: overflow the stack
fn recurse(depth: u64) -> u64 {
    let pad = black_box([depth; 64]);
    let next = black_box(depth).wrapping_add(1);
    black_box(pad[0]).wrapping_add(recurse(next))
}
