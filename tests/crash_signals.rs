//! Process-level tests for the #380 crash-observability module.
//!
//! These run the real `crash_probe` binary as a subprocess. That is not incidental — the claims
//! under test are only meaningful about a whole process:
//!
//! - "the client can start" cannot be tested from inside a test process that already started;
//! - "a stack overflow still prints std's loud message" cannot be tested in-process, because the
//!   test runner would die with it;
//! - "a segfault leaves a record and still core-dumps" likewise.
//!
//! The first version of this fix shipped a binary that panicked on its second statement
//! ("Attempted to register forbidden signal 11") with a fully green `cargo test --lib`, because the
//! unit tests never called the install path. These tests close that hole.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run the probe in `mode` with a private crash dir; return (output, crash-log contents).
fn run_probe(mode: &str) -> (Output, String, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "eqoxide-probe-{}-{}-{}",
        mode,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).expect("create probe crash dir");

    let output = Command::new(env!("CARGO_BIN_EXE_crash_probe"))
        .arg(mode)
        .env("EQOXIDE_CRASH_DIR", &dir)
        // Keep the probe's own core dumps out of the way; we assert on exit status, not the dump.
        .env("RUST_BACKTRACE", "0")
        .output()
        .expect("run crash_probe");

    let log = read_only_crash_log(&dir);
    (output, log, dir)
}

/// The probe is the only writer in its private dir, so there is exactly one `crash-<pid>.log`.
fn read_only_crash_log(dir: &Path) -> String {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return String::new();
    };
    let mut out = String::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with("crash-") && name.ends_with(".log") {
            out.push_str(&std::fs::read_to_string(e.path()).unwrap_or_default());
        }
    }
    out
}

/// FINDING 1 (the one that made the first PR unmergeable): the binary must START.
///
/// The previous implementation called `signal_hook::low_level::register(SIGSEGV, ..)`, which panics
/// on a forbidden signal, so `eqoxide --help` died with exit 101 before parsing a single argument.
/// Every unit test was green. This test would have caught it.
#[test]
fn the_binary_starts_with_crash_handlers_installed() {
    let (out, _log, dir) = run_probe("startup");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "installing crash handlers must not prevent the process from starting.\n\
         status: {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status
    );
    assert!(stdout.contains("PROBE STARTED OK"), "stdout: {stdout}\nstderr: {stderr}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// FINDING 2 (the trap in the "obvious" fix): a stack overflow on a worker thread must STILL print
/// std's loud `has overflowed its stack` message.
///
/// std installs its SIGSEGV/SIGBUS handler with `SA_ONSTACK` on a per-thread `sigaltstack`; that is
/// the only reason this message exists at all. Installing our own handler WITHOUT `SA_ONSTACK` (as
/// `signal_hook::register_signal_unchecked` would) means the kernel delivers the fault on the
/// exhausted stack, it re-faults instantly, and the process dies with NO output whatsoever — a
/// silent 139 where main gives a loud 134. That would make this module a *manufacturer* of the very
/// bug it exists to prevent.
///
/// So: assert the loud message survives our handler, AND that our record is there too.
#[test]
fn a_worker_thread_stack_overflow_stays_loud_and_also_leaves_our_record() {
    let (out, log, dir) = run_probe("stack-overflow");
    let stderr = String::from_utf8_lossy(&out.stderr);

    assert!(
        stderr.contains("has overflowed its stack"),
        "std's stack-overflow message MUST survive our handler — losing it would make this module \
         manufacture silent deaths.\nstatus: {:?}\nstderr: {stderr}\ncrash log: {log:?}",
        out.status
    );
    assert!(
        stderr.contains("probe-overflow"),
        "the message must still name the overflowing thread.\nstderr: {stderr}"
    );
    assert!(
        log.contains("FATAL SIGNAL SIGSEGV"),
        "our own durable record must ALSO be written.\nstderr: {stderr}\ncrash log: {log:?}"
    );
    assert!(
        !out.status.success(),
        "a stack overflow must still be fatal, not swallowed. status: {:?}",
        out.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// The demonstrated real-world failure mode for this binary (7 SIGSEGVs in `coredumpctl`, mesa /
/// wayland-egl frames): a fault from native code, which is NOT a Rust panic and never runs the
/// panic hook. It must leave a durable record, and must still die by the signal so the OS core dump
/// still happens.
#[test]
fn a_segfault_leaves_a_durable_record_and_still_dies_by_the_signal() {
    use std::os::unix::process::ExitStatusExt;

    let (out, log, dir) = run_probe("segv");
    assert!(
        log.contains("FATAL SIGNAL SIGSEGV"),
        "a segfault must leave a record — this is the failure mode #380's issue text is about.\n\
         status: {:?}\ncrash log: {log:?}",
        out.status
    );
    assert!(
        log.contains("pid="),
        "the record must carry the pid (concurrent clients): {log:?}"
    );
    assert_eq!(
        out.status.signal(),
        Some(libc::SIGSEGV),
        "the process must still die BY the signal, so the OS core dump is unchanged. status: {:?}",
        out.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `abort()` (a failed C++ assertion in a driver, a Rust `abort`-on-panic, a double panic) is not a
/// Rust panic either.
#[test]
fn an_abort_leaves_a_durable_record() {
    let (out, log, dir) = run_probe("abort");
    assert!(
        log.contains("FATAL SIGNAL SIGABRT"),
        "abort() must leave a record.\nstatus: {:?}\ncrash log: {log:?}",
        out.status
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// A panic on a non-main thread does not kill the process — it just quietly dies. That is the exact
/// scenario #380 flags. The process must survive (we do NOT force an exit — see the module docs)
/// AND the panic must be durably recorded with the thread's name.
#[test]
fn a_worker_thread_panic_is_recorded_without_killing_the_process() {
    let (out, log, dir) = run_probe("panic-worker");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("PROBE SURVIVED WORKER PANIC"),
        "the panic hook must not change whether the process survives a worker panic \
         (nav_planner's graceful degradation depends on it).\nstdout: {stdout}"
    );
    assert!(
        log.contains("PANIC thread='probe-worker'"),
        "the worker panic must be durably recorded, naming the thread.\ncrash log: {log:?}"
    );
    assert!(
        log.contains("probe worker panic"),
        "the record must carry the panic message.\ncrash log: {log:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
