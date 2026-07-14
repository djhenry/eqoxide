//! Crash and shutdown observability (#380 — "the client must never die without saying why").
//!
//! The agent-honesty invariant (see `.claude/skills/agent-fleet/SKILL.md`) ranks a silent-wrong-
//! answer bug above a loud crash, because the driving agent has no independent channel to
//! reality — whatever the client reports (or fails to report) *is* the agent's world. A process
//! that vanishes without a word is the purest form of that failure: the agent cannot even tell
//! "the client died" from "the network died" from "the world hung" (#371).
//!
//! This module closes that hole for every way this process is known (or suspected) to be able to
//! die, verified against this exact binary's own crash history on this box
//! (`coredumpctl list eqoxide` shows seven real `SIGSEGV`s — see the #380 PR body):
//!
//! 1. **Rust panics, on any thread.** A panic hook that logs the THREAD NAME, message, and
//!    source location — through `tracing` (captured whenever stderr is redirected, e.g. the
//!    documented `dev-run.sh` / launch commands) **and** to a durable, launch-method-independent
//!    crash-log file, so a `setsid`/detached launch that throws stdout away still leaves a
//!    record. **Deliberately does not force `process::exit()`** — see "Why the hook doesn't
//!    escalate to a full process exit" below.
//! 2. **Fatal OS signals** (`SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGABRT`/`SIGFPE`). These are NOT Rust
//!    panics — a GPU-driver or other FFI fault never runs the panic hook — and are the actual,
//!    demonstrated failure mode in this binary's `coredumpctl` history (mesa/wayland-egl frames
//!    in the crashing stack). The handler is async-signal-safe (no allocation, no locks: a raw
//!    `libc::write` to an fd opened once at startup) and then restores the OS default
//!    disposition so a normal core dump still happens — we only ADD a log line, we don't change
//!    what the OS does.
//! 3. **Clean shutdown.** An explicit record at the one place the process exits 0 after a normal
//!    camp/close, so the *absence* of that line is itself diagnostic ("this run did not end
//!    cleanly").
//! 4. **A heartbeat.** A best-effort periodic timestamp write, so a post-mortem can tell an
//!    OOM-kill (heartbeat recent, no panic/signal/clean-shutdown record — `SIGKILL` cannot be
//!    caught, full stop) apart from an internal fault (a record is present) or a process that was
//!    already wedged long before it died (heartbeat stale).
//!
//! ## Why the hook doesn't escalate to a full process exit
//!
//! The issue text asks for a hook that "logs before dying," and by default a Rust panic on a
//! **non-main** thread does NOT kill the process — it prints and lets only that thread die,
//! leaving every other subsystem running. That sounds like exactly the "kill or wedge... without
//! the usual output" risk #380 calls out, and the tempting fix is to make the hook call
//! `process::exit()` unconditionally so every panic becomes a full, loud death.
//!
//! That would regress a graceful-degradation mechanism this codebase already ships deliberately:
//! [`crate::eq_net::nav_planner::Planner`] documents that an earlier version of *its* worker
//! thread panicking silently froze `nav_state` at `planning` forever — "strictly worse than the
//! crash it replaced" — and the fix was NOT to prevent the crash, but to *detect* the dead worker
//! (`Planner::is_dead`) and report it honestly (`planner_dead`) while the rest of the session
//! (character connection, HTTP API, other subsystems) keeps running. Forcing a full process exit
//! on any panic would take the whole client down for a fault that subsystem already turns into an
//! honest, scoped failure. The same is true of ordinary per-request panics: tokio's task executor
//! already isolates a panicking async task so one bad HTTP request doesn't take the server thread
//! down.
//!
//! So this hook's job is narrower and unconditional: **make sure every panic, on every thread,
//! for every subsystem, is durably logged with enough context to identify it** — independent of
//! whether that subsystem has (or lacks) its own graceful-degradation path. What the process does
//! *after* a panic — keep running degraded, or actually die — is left to the existing per-
//! subsystem logic (or, for the top-level main thread, Rust's own unwind-to-`exit(101)` default,
//! unchanged by this hook).

use std::io::Write;
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, Ordering};
#[cfg(test)]
use std::sync::Mutex;

/// Fixed, launch-method-independent path for the crash/shutdown record. Deliberately NOT
/// `/tmp/eqoxide.log` — that file is truncated by `dev-run.sh` on every relaunch and is only
/// ever populated at all if the caller happens to redirect stdout/stderr there (a `setsid`
/// launch, a desktop entry, or a systemd unit may not). This file lives under the user's cache
/// dir and is only ever appended to, so a post-mortem can inspect it long after the process is
/// gone, regardless of how the client was started.
pub fn crash_log_path() -> PathBuf {
    if let Some(p) = test_override_path() {
        return p;
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("eqoxide")
        .join("crash.log")
}

/// Test-only override so unit tests never touch the real `~/.cache/eqoxide/crash.log` (and so
/// parallel tests don't interleave writes into the same file). Guarded by [`TEST_LOCK`] in every
/// test that uses it, since it's process-global.
#[cfg(test)]
static TEST_PATH_OVERRIDE: Mutex<Option<PathBuf>> = Mutex::new(None);

#[cfg(test)]
fn test_override_path() -> Option<PathBuf> {
    TEST_PATH_OVERRIDE.lock().unwrap().clone()
}
#[cfg(not(test))]
fn test_override_path() -> Option<PathBuf> {
    None
}

#[cfg(test)]
fn set_test_override_path(p: Option<PathBuf>) {
    *TEST_PATH_OVERRIDE.lock().unwrap() = p;
}

fn open_crash_log() -> Option<std::fs::File> {
    let path = crash_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        Ok(f) => Some(f),
        Err(e) => {
            // Best-effort: if even this fails, `tracing::error!` (stderr) is still attempted by
            // the caller, so we're not making things worse — just not as durable as we'd like.
            eprintln!("crash: could not open durable crash log {}: {e}", path.display());
            None
        }
    }
}

/// Append one line to the durable crash log. Best-effort and infallible from the caller's
/// perspective — never panics, since this is called from inside the panic hook itself.
fn append_line(msg: &str) {
    if let Some(mut f) = open_crash_log() {
        let _ = writeln!(f, "{msg}");
        let _ = f.flush();
    }
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Pure formatting for a panic record — split out from the hook itself so it's unit-testable
/// without needing to install a global panic hook or panic at all.
fn format_panic_line(ts: u64, thread_name: &str, location: &str, payload: &str) -> String {
    format!("[{ts}] PANIC thread='{thread_name}' at {location}: {payload}")
}

fn format_clean_shutdown_line(ts: u64, pid: i32) -> String {
    format!("[{ts}] CLEAN SHUTDOWN pid={pid}")
}

/// Install the panic hook. Wraps (does not replace) the previous hook so default output — the
/// normal Rust panic message, including thread name, that anyone watching stderr already sees —
/// is unchanged; this only ADDS the durable, structured record described in the module docs.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);

        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        let location = info
            .location()
            .map(|l| l.to_string())
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };

        let line = format_panic_line(now_epoch_secs(), thread_name, &location, &payload);

        // Through the normal logging pipeline (captured whenever stderr is redirected — see the
        // `build-run` skill's documented launch commands)...
        tracing::error!(target: "eqoxide::crash", "{line}");
        // ...AND to the durable, launch-independent crash log (see module docs: a launch that
        // throws stdout/stderr away still leaves this record).
        append_line(&line);
    }));
}

/// Call at the one place the process is about to exit 0 after a normal, requested shutdown
/// (POST /exit, window close, clean camp). Its presence as the LAST line in the crash log is
/// what makes the log's ABSENCE of such a line, after a run that's no longer running, diagnostic
/// of an unclean death.
pub fn log_clean_shutdown() {
    let pid = std::process::id() as i32;
    let line = format_clean_shutdown_line(now_epoch_secs(), pid);
    tracing::info!(target: "eqoxide::crash", "{line}");
    append_line(&line);
}

// ---------------------------------------------------------------------------------------------
// Fatal signal handling
// ---------------------------------------------------------------------------------------------

/// Raw fd for the crash log, opened once at startup and kept open for the life of the process so
/// the signal handler (which must not allocate or lock) has something ready to write to. `-1`
/// means "failed to open" — the handler checks and skips the write rather than passing a bad fd
/// to `write(2)`.
static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

fn init_crash_fd() -> RawFd {
    match open_crash_log() {
        Some(f) => {
            let fd = f.as_raw_fd();
            // Leak the File so the fd stays open for the process's lifetime — a signal handler
            // can fire at any point, including after main() would otherwise have dropped a
            // locally-scoped File and closed the fd.
            std::mem::forget(f);
            fd
        }
        None => -1,
    }
}

/// (signal constant, human-readable name) for every signal we install a handler for. These are
/// the signals that terminate the process regardless of what our Rust code does — there is no
/// "keep running gracefully" option for a segfault. The only thing we can add is a log line
/// before the OS does what it was always going to do.
fn fatal_signals() -> [(i32, &'static str); 5] {
    [
        (libc::SIGSEGV, "SIGSEGV"),
        (libc::SIGBUS, "SIGBUS"),
        (libc::SIGILL, "SIGILL"),
        (libc::SIGABRT, "SIGABRT"),
        (libc::SIGFPE, "SIGFPE"),
    ]
}

/// Async-signal-safe: writes a fixed-format record ("FATAL SIGNAL <NAME> pid=<PID>\n") to `fd`
/// using only a stack buffer and the raw `write(2)` syscall. No allocation, no locks, no
/// `format!`/`tracing` — none of those are safe to call from inside a signal handler (POSIX
/// async-signal-safety; see `signal_hook::low_level::register`'s safety docs).
fn write_fatal_record(fd: RawFd, signal_name: &str) {
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 160];
    let mut n = 0usize;
    n += copy_bytes(&mut buf[n..], b"FATAL SIGNAL ");
    n += copy_bytes(&mut buf[n..], signal_name.as_bytes());
    n += copy_bytes(&mut buf[n..], b" pid=");
    // SAFETY: getpid() takes no arguments and cannot fail; async-signal-safe per POSIX.
    let pid = unsafe { libc::getpid() } as u32;
    n += write_u32_decimal(&mut buf[n..], pid);
    n += copy_bytes(&mut buf[n..], b"\n");
    // SAFETY: `buf[..n]` is a valid, initialized, in-bounds slice for the duration of this call;
    // `write` is async-signal-safe per POSIX. Short writes are ignored (best-effort, and we
    // cannot retry safely/usefully from inside a signal handler for a fatal signal).
    unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, n);
    }
}

/// Copies as much of `src` as fits into `dst`, returns the number of bytes written. Stack-only,
/// no allocation — safe to call from a signal handler.
fn copy_bytes(dst: &mut [u8], src: &[u8]) -> usize {
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
    len
}

/// Formats `v` as decimal ASCII into `dst`, returns the number of bytes written. Stack-only, no
/// allocation, no `format!` — safe to call from a signal handler.
fn write_u32_decimal(dst: &mut [u8], mut v: u32) -> usize {
    if dst.is_empty() {
        return 0;
    }
    if v == 0 {
        dst[0] = b'0';
        return 1;
    }
    let mut tmp = [0u8; 10]; // u32::MAX has 10 decimal digits
    let mut i = 0;
    while v > 0 && i < tmp.len() {
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        i += 1;
    }
    let len = i.min(dst.len());
    for k in 0..len {
        dst[k] = tmp[i - 1 - k];
    }
    len
}

/// Install handlers for the fatal signals listed in [`fatal_signals`]. Each handler is
/// async-signal-safe (see [`write_fatal_record`]) and, after logging, calls
/// `signal_hook::low_level::emulate_default_handler` so the OS's normal behavior (terminate,
/// core dump if enabled) proceeds exactly as it would have without this handler installed — we
/// only add a log line in front of it.
///
/// Known limitation: registering our own `SIGSEGV`/`SIGBUS` handler here may override Rust std's
/// built-in stack-overflow guard-page handler (which prints "thread '...' has overflowed its
/// stack" and runs on an alternate signal stack). We do not install with `SA_ONSTACK`, so a
/// genuine stack overflow could plausibly re-fault while our handler is still trying to run on
/// the same (exhausted) stack, losing both messages. This is a disclosed trade-off, not a proven
/// regression — the demonstrated failure mode in this binary's crash history (GPU-driver
/// `SIGSEGV` during zone/mesh upload; see the PR body) is unrelated to stack depth.
fn install_signal_handlers() {
    let fd = init_crash_fd();
    CRASH_FD.store(fd, Ordering::Relaxed);
    if fd < 0 {
        tracing::warn!("crash: durable crash log unavailable — fatal-signal records will be lost");
    }
    for (sig, name) in fatal_signals() {
        // SAFETY: the registered action only performs async-signal-safe operations —
        // `write_fatal_record` (raw `write(2)` to an fd opened at startup, no allocation, no
        // locks) and `emulate_default_handler` (documented async-signal-safe by signal-hook).
        let result = unsafe {
            signal_hook::low_level::register(sig, move || {
                write_fatal_record(CRASH_FD.load(Ordering::Relaxed), name);
                let _ = signal_hook::low_level::emulate_default_handler(sig);
            })
        };
        if let Err(e) = result {
            tracing::warn!("crash: failed to install handler for {name} ({sig}): {e}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------------------------

/// Fixed path for the heartbeat file, sibling to the crash log.
pub fn heartbeat_path() -> PathBuf {
    crash_log_path()
        .parent()
        .map(|p| p.join("heartbeat"))
        .unwrap_or_else(|| PathBuf::from("eqoxide-heartbeat"))
}

/// Spawn a background thread that overwrites the heartbeat file with the current timestamp every
/// few seconds for the life of the process. Recommended by #380 as a way to distinguish an
/// OOM-kill (`SIGKILL` cannot be caught or logged — the heartbeat will simply stop, recently,
/// with no panic/signal/clean-shutdown record) from a process that was already wedged well
/// before it died (heartbeat stale for a long time before the process vanished) or an internal
/// fault (a panic/signal record is present). This is ordinary blocking I/O on a normal thread —
/// NOT signal-handler context — so it can use `tracing`/`format!`/locks freely.
fn spawn_heartbeat_thread() {
    let interval = std::time::Duration::from_secs(5);
    let result = std::thread::Builder::new()
        .name("crash-heartbeat".into())
        .spawn(move || loop {
            let path = heartbeat_path();
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let _ = std::fs::write(&path, now_epoch_secs().to_string());
            std::thread::sleep(interval);
        });
    if let Err(e) = result {
        tracing::warn!("crash: failed to spawn heartbeat thread: {e}");
    }
}

/// Install everything this module provides: the panic hook, the fatal-signal handlers, and the
/// heartbeat thread. Call once, as early as possible in `main()` — before any other thread is
/// spawned, so nothing can panic or fault before the hook is live.
pub fn install() {
    install_panic_hook();
    install_signal_handlers();
    spawn_heartbeat_thread();
}

#[cfg(test)]
mod tests {
    use super::*;

    // Guards every test that mutates process-global state (the panic hook, or
    // `TEST_PATH_OVERRIDE`) so `cargo test`'s default parallel execution can't interleave them.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn crash_log_path_is_stable_and_under_a_cache_dir() {
        let _guard = TEST_LOCK.lock().unwrap();
        set_test_override_path(None);
        let p1 = crash_log_path();
        let p2 = crash_log_path();
        assert_eq!(p1, p2, "path must be deterministic across calls");
        assert!(p1.ends_with("eqoxide/crash.log") || p1.ends_with("eqoxide\\crash.log"));
    }

    #[test]
    fn format_panic_line_contains_thread_name_message_and_location() {
        let line = format_panic_line(1_700_000_000, "nav-planner", "src/foo.rs:12:5", "index out of bounds");
        assert!(line.contains("nav-planner"), "must name the thread: {line}");
        assert!(line.contains("src/foo.rs:12:5"), "must carry the location: {line}");
        assert!(line.contains("index out of bounds"), "must carry the message: {line}");
        assert!(line.starts_with("[1700000000] PANIC"), "must be a recognizable PANIC record: {line}");
    }

    #[test]
    fn format_clean_shutdown_line_is_distinguishable_from_a_panic() {
        let line = format_clean_shutdown_line(1_700_000_000, 4242);
        assert!(line.contains("CLEAN SHUTDOWN"));
        assert!(line.contains("4242"));
        assert!(!line.contains("PANIC"));
    }

    #[test]
    fn write_u32_decimal_round_trips_representative_values() {
        for v in [0u32, 1, 9, 10, 42, 4242, 65535, u32::MAX] {
            let mut buf = [0u8; 16];
            let n = write_u32_decimal(&mut buf, v);
            let s = std::str::from_utf8(&buf[..n]).unwrap();
            assert_eq!(s.parse::<u32>().unwrap(), v, "round-trip failed for {v}");
        }
    }

    #[test]
    fn copy_bytes_truncates_to_the_destination_and_returns_bytes_written() {
        let mut dst = [0u8; 4];
        let n = copy_bytes(&mut dst, b"hello");
        assert_eq!(n, 4);
        assert_eq!(&dst, b"hell");
    }

    /// The strong demonstration the #380 task asked for: deliberately panic a NON-main
    /// ("worker") thread with the real hook installed, and confirm the durable crash log ends up
    /// with a record naming that thread and its message — exactly the scenario #380 flags as
    /// dangerous ("a panic on a non-main thread can kill or wedge the process without the main
    /// thread's usual output").
    ///
    /// MUTATION CHECK (performed manually, not automatable in-line — see the #380 PR body): with
    /// `install_panic_hook()`'s body emptied (hook installed but a no-op), this test goes RED —
    /// the crash log file is never created/written, so the final `assert!(log.contains(...))`
    /// fails. Restoring the real body turns it back GREEN. This proves the test is actually
    /// exercising the hook's logging side effect, not just observing an unrelated default.
    #[test]
    fn panicking_worker_thread_lands_a_record_in_the_durable_crash_log() {
        let _guard = TEST_LOCK.lock().unwrap();

        let dir = tempfile_dir();
        let log_path = dir.join("crash.log");
        set_test_override_path(Some(log_path.clone()));

        let previous = std::panic::take_hook();
        install_panic_hook();

        let handle = std::thread::Builder::new()
            .name("test-worker-thread".into())
            .spawn(|| {
                panic!("synthetic panic for #380 verification");
            })
            .unwrap();
        // A panic on a non-main thread does NOT crash the test process — `join` just returns
        // `Err`, which is exactly the "silently degrades, doesn't kill the process" behavior
        // #380 is concerned about being invisible.
        let joined = handle.join();
        assert!(joined.is_err(), "the worker thread should have panicked");

        std::panic::set_hook(previous);
        set_test_override_path(None);

        let contents = std::fs::read_to_string(&log_path)
            .unwrap_or_else(|e| panic!("crash log was never written at {}: {e}", log_path.display()));
        assert!(contents.contains("test-worker-thread"),
            "crash log must name the panicking thread, got: {contents}");
        assert!(contents.contains("synthetic panic for #380 verification"),
            "crash log must carry the panic message, got: {contents}");
        assert!(contents.contains("PANIC"), "must be tagged as a PANIC record, got: {contents}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clean_shutdown_writes_a_line_distinct_from_any_panic_record() {
        let _guard = TEST_LOCK.lock().unwrap();

        let dir = tempfile_dir();
        let log_path = dir.join("crash.log");
        set_test_override_path(Some(log_path.clone()));

        log_clean_shutdown();

        set_test_override_path(None);

        let contents = std::fs::read_to_string(&log_path).unwrap();
        assert!(contents.contains("CLEAN SHUTDOWN"), "got: {contents}");
        assert!(!contents.contains("PANIC"), "got: {contents}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "eqoxide-crash-test-{}-{}",
            std::process::id(),
            now_epoch_secs()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }
}
