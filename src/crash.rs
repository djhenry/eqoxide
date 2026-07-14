//! Crash and shutdown observability (#380 — "the client must never die without saying why").
//!
//! The agent-honesty invariant ranks a silent-wrong-answer bug above a loud crash, because the
//! driving agent has no independent channel to reality — whatever the client reports (or fails to
//! report) *is* the agent's world. A process that vanishes without a word is the purest form of
//! that failure: the agent cannot tell "the client died" from "the network died" from "the world
//! hung" (#371).
//!
//! This module makes every *known* way this process can die leave a durable record.
//!
//! ## The three rules this module is built around
//!
//! 1. **Never make an existing loud failure quiet.** This is not a slogan; it is the specific trap
//!    that sank the first version of this fix. See "Why we don't use signal-hook" below.
//! 2. **Every record is per-instance.** Agents routinely run several clients at once on distinct
//!    `--api-port`s. A shared log or heartbeat file is last-writer-wins, and a post-mortem would
//!    happily attribute client A's death to client B's still-beating heart. Every path and every
//!    line carries the pid.
//! 3. **Every intentional exit is labelled.** A wedged render loop that gets force-exited by the
//!    `/v1/lifecycle/exit` watchdog must not be indistinguishable from an OOM-kill — that would be
//!    an agent-honesty violation *inside* the agent-honesty fix.
//!
//! ## What is covered
//!
//! - **Rust panics, on any thread** — [`install_panic_hook`]. Logs thread name, location, message,
//!   and pid, through `tracing` *and* to the durable per-pid crash log.
//! - **Fatal OS signals** (`SIGSEGV`/`SIGBUS`/`SIGILL`/`SIGFPE`/`SIGABRT`) — [`install_signal_handlers`].
//!   These are NOT Rust panics: a GPU-driver fault never runs the panic hook, and this binary has a
//!   real `SIGSEGV` history (`coredumpctl list eqoxide` — 7 crashes with mesa/wayland-egl frames).
//! - **Every intentional exit** — [`exit`] / [`log_exit`]. Including the ones that fire precisely
//!   when something is already wrong (the render-loop watchdog).
//! - **A heartbeat** — so a post-mortem can distinguish an uncatchable `SIGKILL`/OOM-kill from a
//!   process that was already wedged long before it died.
//!
//! ## Why we don't use `signal-hook` for the fatal signals
//!
//! `signal_hook::low_level::register` **panics** on `SIGSEGV`/`SIGILL`/`SIGFPE` (they're on its
//! `FORBIDDEN_IMPL` list), and its `register_signal_unchecked` escape hatch installs with
//! `SA_RESTART | SA_SIGINFO` and **never `SA_ONSTACK`**.
//!
//! That second point is the dangerous one. Rust std installs its own `SIGSEGV`/`SIGBUS` handler
//! *with* `SA_ONSTACK`, on a per-thread `sigaltstack`, and that is the only reason a stack overflow
//! prints `thread '...' has overflowed its stack` instead of dying mute. Overwrite that disposition
//! without `SA_ONSTACK` and the kernel delivers the `SIGSEGV` on the *already-exhausted* stack, it
//! immediately re-faults, and **neither std's message nor our own record ever runs** — a silent
//! `exit 139` where `main` today gives a loud `exit 134`. A fix for "the client dies silently" that
//! *manufactures* a new class of silent death is the single worst outcome available here.
//!
//! So we install by hand with `libc::sigaction` and:
//! - `SA_ONSTACK`, so our handler runs on the alternate signal stack even when the real stack is
//!   gone — the same protection std relies on;
//! - `SA_SIGINFO`, so we can pass `(sig, info, ctx)` through unchanged;
//! - **chaining**: after writing our record we call whatever handler was installed before us
//!   (normally std's). std's handler still prints its stack-overflow message; std's non-overflow
//!   path still restores `SIG_DFL` and lets the fault re-raise, so **core dumps still happen**.
//!   We only ever *add* a line in front of the behavior that was already there.
//!
//! This is verified, not asserted: `tests/crash_signals.rs` runs the real `crash_probe` binary as a
//! subprocess and asserts that a worker-thread stack overflow still prints std's loud message
//! *and* leaves our record — and that the binary starts at all.
//!
//! ## Why the panic hook does not force `process::exit`
//!
//! By default a panic on a **non-main** thread kills only that thread. Escalating that to a whole-
//! process exit would regress graceful degradation this codebase ships deliberately:
//! [`crate::eq_net::nav_planner::Planner`] documents that its worker thread panicking used to
//! freeze `nav_state` forever — and the fix was not to prevent the crash but to *detect* the dead
//! worker (`Planner::is_dead`) and report it honestly while the rest of the session keeps running.
//! tokio likewise isolates a panicking task so one bad HTTP request doesn't take the server down.
//!
//! So the hook's job is narrower and unconditional: **every panic, on every thread, is durably
//! logged with enough context to identify it.** What the process does *afterwards* is left to the
//! existing per-subsystem logic. A partial-thread-death "wedge" is therefore still possible after
//! this change — what is fixed is that it is no longer *invisible*.

use std::io::Write;
use std::os::fd::{IntoRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering};
#[cfg(test)]
use std::sync::{Mutex, MutexGuard};

// ---------------------------------------------------------------------------------------------
// Paths — per-instance (pid), because several clients run at once (#380 review, finding 4)
// ---------------------------------------------------------------------------------------------

/// Env override for the crash directory. Used by the subprocess tests so they never touch the real
/// `~/.cache/eqoxide/crash/`, and available as an operational escape hatch.
pub const CRASH_DIR_ENV: &str = "EQOXIDE_CRASH_DIR";

/// Directory holding all crash/heartbeat records. Deliberately NOT `/tmp/eqoxide.log`: that file is
/// truncated by `dev-run.sh` on every relaunch, and only exists at all if the caller happened to
/// redirect stderr there — a `setsid`/detached launch may not.
pub fn crash_dir() -> PathBuf {
    if let Ok(d) = std::env::var(CRASH_DIR_ENV) {
        if !d.is_empty() {
            return PathBuf::from(d);
        }
    }
    dirs::cache_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("eqoxide")
        .join("crash")
}

/// Per-pid crash log. Two clients running concurrently write to two different files, so "did this
/// instance exit cleanly?" is answerable — with a single shared file it is not (finding 4).
pub fn crash_log_path() -> PathBuf {
    crash_dir().join(format!("crash-{}.log", std::process::id()))
}

/// Per-pid heartbeat. Same reasoning: a shared heartbeat is last-writer-wins, so a second live
/// client would keep a dead client's heartbeat "fresh" and the post-mortem would conclude the dead
/// one had just been SIGKILLed. That defeats the file's entire purpose in the *normal* case.
pub fn heartbeat_path() -> PathBuf {
    crash_dir().join(format!("heartbeat-{}", std::process::id()))
}

// ---------------------------------------------------------------------------------------------
// Durable append
// ---------------------------------------------------------------------------------------------

/// Raw fd for this instance's crash log, opened once at [`install`] and intentionally leaked so it
/// stays valid for the life of the process — a signal handler can fire at any time, and must never
/// allocate, lock, or open a file. `-1` = unavailable.
static CRASH_FD: AtomicI32 = AtomicI32::new(-1);

/// Bytes written so far, for the size cap. An agent hammering an endpoint that panics in a tokio
/// task would otherwise append a record per request, forever (finding 5).
static BYTES_WRITTEN: AtomicU64 = AtomicU64::new(0);
static CAP_NOTICE_WRITTEN: AtomicBool = AtomicBool::new(false);

/// 1 MiB is thousands of records — far more than any post-mortem needs, and small enough that a
/// pathological panic loop can't fill the disk.
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// False once any durable write has failed. Surfaced by [`crash_log_healthy`] so a caller can tell
/// "no record" from "we couldn't write one" (finding 6).
static LOG_HEALTHY: AtomicBool = AtomicBool::new(true);

/// Whether the durable crash log is still known-good. `false` means a write failed (disk full,
/// permissions) and records may be missing — i.e. the absence of a record is NOT evidence of a
/// clean run.
pub fn crash_log_healthy() -> bool {
    LOG_HEALTHY.load(Ordering::Relaxed)
}

/// Poisoning a lock must not cascade one real failure into a pile of unrelated ones (finding 7):
/// a single genuine failure inside a guarded test would otherwise poison the mutex and turn every
/// other test using it into a spurious `PoisonError`, burying the real message.
#[cfg(test)]
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn mark_unhealthy(what: &str) {
    if LOG_HEALTHY.swap(false, Ordering::Relaxed) {
        // Only complain once. This goes to tracing (stderr) — which the module docs note may be
        // discarded by a detached launch. That is precisely why we ALSO expose
        // `crash_log_healthy()` programmatically rather than relying on anyone reading stderr.
        tracing::error!(
            target: "eqoxide::crash",
            "durable crash log is UNHEALTHY ({what}) — crash records may be lost; \
             absence of a record is NOT evidence of a clean exit"
        );
    }
}

/// Append one line to the durable crash log. Best-effort and infallible from the caller's
/// perspective — never panics, since it is called from inside the panic hook itself.
///
/// Writes through the pre-opened fd with a single `write(2)` when installed (the file is `O_APPEND`,
/// so a single write is atomic and concurrent writers cannot interleave a partial line), and falls
/// back to opening the file when not installed (unit tests, and any caller that logs before
/// `install()`).
fn append_line(msg: &str) {
    if BYTES_WRITTEN.load(Ordering::Relaxed) >= MAX_LOG_BYTES {
        // Say once that we stopped, so a truncated log is never mistaken for a quiet one.
        if !CAP_NOTICE_WRITTEN.swap(true, Ordering::Relaxed) {
            raw_append("[log capped: further records suppressed]\n");
        }
        return;
    }

    let mut line = String::with_capacity(msg.len() + 1);
    line.push_str(msg);
    line.push('\n');
    BYTES_WRITTEN.fetch_add(line.len() as u64, Ordering::Relaxed);
    raw_append(&line);
}

fn raw_append(line: &str) {
    let fd = CRASH_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        // SAFETY: `fd` is a valid, open, O_APPEND fd owned for the life of the process; the slice is
        // valid for the duration of the call.
        let n = unsafe { libc::write(fd, line.as_ptr() as *const libc::c_void, line.len()) };
        if n < 0 {
            mark_unhealthy("write failed");
        }
        return;
    }
    // Not installed (unit tests / pre-install callers): open, append, close.
    match open_log_for_append() {
        Some(mut f) => {
            if f.write_all(line.as_bytes()).is_err() || f.flush().is_err() {
                mark_unhealthy("fallback write failed");
            }
        }
        None => mark_unhealthy("could not open log"),
    }
}

fn open_log_for_append() -> Option<std::fs::File> {
    let path = crash_log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()
}

fn now_epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn pid() -> u32 {
    std::process::id()
}

// ---------------------------------------------------------------------------------------------
// Record formatting (pure, unit-testable)
// ---------------------------------------------------------------------------------------------

/// Every record carries the pid — without it, records from concurrent clients are unattributable
/// and "is the last line a clean exit?" is meaningless (finding 4).
fn format_panic_line(ts: u64, pid: u32, thread_name: &str, location: &str, payload: &str) -> String {
    format!("[{ts}] pid={pid} PANIC thread='{thread_name}' at {location}: {payload}")
}

fn format_exit_line(ts: u64, pid: u32, reason: &str, code: i32) -> String {
    format!("[{ts}] pid={pid} EXIT reason={reason} code={code}")
}

fn format_instance_line(ts: u64, pid: u32, label: &str) -> String {
    format!("[{ts}] pid={pid} INSTANCE {label}")
}

// ---------------------------------------------------------------------------------------------
// Panic hook
// ---------------------------------------------------------------------------------------------

/// Install the panic hook. Wraps (does not replace) the previous hook, so the normal Rust panic
/// message anyone watching stderr already sees is unchanged; this only ADDS the durable record.
fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        previous(info);

        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>").to_string();
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

        let line = format_panic_line(now_epoch_secs(), pid(), &thread_name, &location, &payload);
        tracing::error!(target: "eqoxide::crash", "{line}");
        append_line(&line);
    }));
}

// ---------------------------------------------------------------------------------------------
// Exit records
// ---------------------------------------------------------------------------------------------

/// Record an intentional exit, with a REASON. Call this at every `process::exit` site.
///
/// The reason matters as much as the record: `/v1/lifecycle/exit`'s 45s watchdog fires exactly when
/// the render loop is already wedged. If that exit were unlabelled, a post-mortem would see "no
/// clean-shutdown line, no panic, no signal, fresh heartbeat" — which this module documents as
/// meaning *OOM-kill*. A wedge would be confidently misreported as an OOM. Labelling it
/// `render-loop-wedged` keeps that honest.
pub fn log_exit(reason: &str, code: i32) {
    let line = format_exit_line(now_epoch_secs(), pid(), reason, code);
    tracing::info!(target: "eqoxide::crash", "{line}");
    append_line(&line);
}

/// Log an intentional exit and then take it. Use in place of bare `std::process::exit`.
pub fn exit(reason: &str, code: i32) -> ! {
    log_exit(reason, code);
    std::process::exit(code)
}

/// The ordinary, fully-clean shutdown (camp completed, render loop exited on its own).
pub fn log_clean_shutdown() {
    log_exit("clean", 0);
}

/// Record what this instance *is*, so a directory of per-pid logs is navigable — which pid was the
/// client on api_port 8901, which config it ran. Call once the port is actually bound.
pub fn log_instance(label: &str) {
    let line = format_instance_line(now_epoch_secs(), pid(), label);
    tracing::info!(target: "eqoxide::crash", "{line}");
    append_line(&line);
}

// ---------------------------------------------------------------------------------------------
// Fatal signal handling
// ---------------------------------------------------------------------------------------------

/// The signals that terminate the process no matter what our Rust code does. There is no "keep
/// running gracefully" option for a segfault; the only thing we can add is a record in front of the
/// death the OS was always going to deliver.
const FATAL_SIGNALS: [libc::c_int; 5] =
    [libc::SIGSEGV, libc::SIGBUS, libc::SIGILL, libc::SIGFPE, libc::SIGABRT];

/// Async-signal-safe name lookup — no allocation, no formatting.
fn signal_name(sig: libc::c_int) -> &'static str {
    match sig {
        libc::SIGSEGV => "SIGSEGV",
        libc::SIGBUS => "SIGBUS",
        libc::SIGILL => "SIGILL",
        libc::SIGFPE => "SIGFPE",
        libc::SIGABRT => "SIGABRT",
        _ => "SIGNAL",
    }
}

/// Previous `sigaction` for each signal, so our handler can chain to it (normally std's). Indexed by
/// signal number; each entry is a leaked `Box<libc::sigaction>` so the pointer stays valid forever
/// and the handler never has to allocate or lock to read it.
const MAX_SIGNAL: usize = 32;
#[allow(clippy::declare_interior_mutable_const)]
const NULL_ACTION: AtomicPtr<libc::sigaction> = AtomicPtr::new(std::ptr::null_mut());
static PREV_ACTION: [AtomicPtr<libc::sigaction>; MAX_SIGNAL] = [NULL_ACTION; MAX_SIGNAL];

/// Writes `FATAL SIGNAL <NAME> pid=<PID>\n` using only a stack buffer and raw `write(2)`.
///
/// Async-signal-safe: no allocation, no locks, no `format!`, no `tracing` — none of which are legal
/// in a signal handler (POSIX async-signal-safety). This runs on the alternate signal stack (we
/// install with `SA_ONSTACK`), so it is still safe when the thread's real stack is exhausted.
fn write_fatal_record(fd: RawFd, signal_name: &str) {
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 160];
    let mut n = 0usize;
    n += copy_bytes(&mut buf[n..], b"FATAL SIGNAL ");
    n += copy_bytes(&mut buf[n..], signal_name.as_bytes());
    n += copy_bytes(&mut buf[n..], b" pid=");
    // SAFETY: getpid() takes no arguments, cannot fail, and is async-signal-safe per POSIX.
    let p = unsafe { libc::getpid() } as u32;
    n += write_u32_decimal(&mut buf[n..], p);
    n += copy_bytes(&mut buf[n..], b"\n");
    // SAFETY: `buf[..n]` is valid and in-bounds; `write` is async-signal-safe. Short writes are
    // ignored — we cannot usefully retry from inside a fatal-signal handler.
    unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, n);
    }
}

/// Copies as much of `src` as fits into `dst`; returns bytes written. Stack-only.
fn copy_bytes(dst: &mut [u8], src: &[u8]) -> usize {
    let len = src.len().min(dst.len());
    dst[..len].copy_from_slice(&src[..len]);
    len
}

/// Formats `v` as decimal ASCII into `dst`; returns bytes written. Stack-only, no `format!`.
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

/// Our handler: write the record, then hand control to whoever was handling this signal before us.
///
/// Chaining is what preserves existing behavior. For a stack overflow, std's handler prints
/// `thread '...' has overflowed its stack` and aborts. For any other fault, std's handler restores
/// `SIG_DFL` and returns, letting the faulting instruction re-run and produce the normal core dump.
/// Either way we have already written our line, and we changed nothing else.
extern "C" fn fatal_signal_handler(
    sig: libc::c_int,
    info: *mut libc::siginfo_t,
    ctx: *mut libc::c_void,
) {
    write_fatal_record(CRASH_FD.load(Ordering::Relaxed), signal_name(sig));
    chain_to_previous(sig, info, ctx);
}

/// Invoke the handler that was installed before ours. If there wasn't one (or it was `SIG_DFL` /
/// `SIG_IGN`), restore the default disposition and re-raise, so the OS does exactly what it would
/// have done without us — including writing a core dump.
fn chain_to_previous(sig: libc::c_int, info: *mut libc::siginfo_t, ctx: *mut libc::c_void) {
    let idx = sig as usize;
    if idx >= MAX_SIGNAL {
        restore_default_and_reraise(sig);
        return;
    }
    let prev = PREV_ACTION[idx].load(Ordering::Relaxed);
    if prev.is_null() {
        restore_default_and_reraise(sig);
        return;
    }
    // SAFETY: `prev` is a leaked Box<libc::sigaction> written once during `install()`, before any
    // signal handler could run; it is never freed or mutated afterwards.
    let action = unsafe { *prev };
    let handler = action.sa_sigaction;
    if handler == libc::SIG_DFL || handler == libc::SIG_IGN {
        restore_default_and_reraise(sig);
        return;
    }
    // SAFETY: the previous disposition was a real handler function. `SA_SIGINFO` tells us which of
    // the two ABIs it uses; we forward the kernel's own arguments unchanged.
    unsafe {
        if action.sa_flags & libc::SA_SIGINFO != 0 {
            let f: extern "C" fn(libc::c_int, *mut libc::siginfo_t, *mut libc::c_void) =
                std::mem::transmute(handler);
            f(sig, info, ctx);
        } else {
            let f: extern "C" fn(libc::c_int) = std::mem::transmute(handler);
            f(sig);
        }
    }
}

/// Put the signal back to its OS default and deliver it again — the process dies exactly as it
/// would have with no handler installed (core dump included).
fn restore_default_and_reraise(sig: libc::c_int) {
    // SAFETY: `sigaction`/`raise` are async-signal-safe; we zero-init the struct and set only
    // `sa_sigaction = SIG_DFL`.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = libc::SIG_DFL;
        libc::sigemptyset(&mut sa.sa_mask);
        libc::sigaction(sig, &sa, std::ptr::null_mut());
        libc::raise(sig);
    }
}

/// Install our fatal-signal handlers with `SA_SIGINFO | SA_ONSTACK | SA_RESTART`, chaining the
/// previous disposition.
///
/// `SA_ONSTACK` is not optional and not a nicety: without it, a stack overflow is delivered on the
/// exhausted stack, re-faults immediately, and dies **completely silently** (`exit 139`, no output
/// at all) — strictly worse than today's `main`, which prints `has overflowed its stack` and exits
/// 134. See the module docs. `tests/crash_signals.rs` proves the loud message survives.
///
/// Returns the number of handlers successfully installed.
fn install_signal_handlers() -> usize {
    let mut installed = 0;
    for sig in FATAL_SIGNALS {
        // SAFETY: `fatal_signal_handler` is `extern "C"` and async-signal-safe (raw `write(2)` to a
        // pre-opened fd + chaining). We declare `SA_SIGINFO` and match its 3-argument ABI.
        let ok = unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = fatal_signal_handler as *const () as usize;
            sa.sa_flags = libc::SA_SIGINFO | libc::SA_ONSTACK | libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);

            let mut old: libc::sigaction = std::mem::zeroed();
            let rc = libc::sigaction(sig, &sa, &mut old);
            if rc == 0 {
                let idx = sig as usize;
                if idx < MAX_SIGNAL {
                    // Leak it: the handler must be able to read this forever without allocating.
                    PREV_ACTION[idx].store(Box::into_raw(Box::new(old)), Ordering::Relaxed);
                }
                true
            } else {
                false
            }
        };
        if ok {
            installed += 1;
        } else {
            tracing::warn!(
                target: "eqoxide::crash",
                "failed to install handler for {} — faults on this signal will not be logged",
                signal_name(sig)
            );
        }
    }
    installed
}

// ---------------------------------------------------------------------------------------------
// Heartbeat
// ---------------------------------------------------------------------------------------------

/// Rewrite this instance's heartbeat file with the current timestamp every few seconds.
///
/// This is the only handle we have on an OOM-kill: `SIGKILL` cannot be caught, logged, or handled
/// by *any* userspace mechanism. A post-mortem reads: a fresh heartbeat with no EXIT/PANIC/FATAL
/// record = killed from outside (OOM); a stale heartbeat = the process was already wedged well
/// before it died. Ordinary blocking I/O on its own thread — not signal context.
fn spawn_heartbeat_thread() {
    let interval = std::time::Duration::from_secs(5);
    let path = heartbeat_path();
    let result = std::thread::Builder::new()
        .name("crash-heartbeat".into())
        .spawn(move || loop {
            let _ = std::fs::write(&path, now_epoch_secs().to_string());
            std::thread::sleep(interval);
        });
    if let Err(e) = result {
        tracing::warn!(target: "eqoxide::crash", "failed to spawn heartbeat thread: {e}");
    }
}

// ---------------------------------------------------------------------------------------------
// install
// ---------------------------------------------------------------------------------------------

/// Install everything: panic hook, fatal-signal handlers, heartbeat. Call once, as early as possible
/// in `main()`, before any other thread is spawned.
pub fn install() {
    let dir = crash_dir();
    if let Err(e) = std::fs::create_dir_all(&dir) {
        mark_unhealthy("could not create crash dir");
        tracing::error!(target: "eqoxide::crash", "cannot create {}: {e}", dir.display());
    }

    match open_log_for_append() {
        Some(f) => {
            // Leak the fd: a signal handler may fire at any moment, including after `main` would
            // have dropped a scoped File and closed it.
            CRASH_FD.store(f.into_raw_fd(), Ordering::Relaxed);
        }
        None => {
            mark_unhealthy("could not open log at install");
        }
    }

    install_panic_hook();
    let n = install_signal_handlers();
    if n != FATAL_SIGNALS.len() {
        tracing::warn!(
            target: "eqoxide::crash",
            "only {n}/{} fatal-signal handlers installed",
            FATAL_SIGNALS.len()
        );
    }
    spawn_heartbeat_thread();

    tracing::info!(
        target: "eqoxide::crash",
        "crash observability installed: {} (log healthy: {})",
        crash_log_path().display(),
        crash_log_healthy()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// Guards tests that mutate process-global state (panic hook, env var, CRASH_FD).
    /// Uses `lock()` (not `.unwrap()`) so one real failure doesn't cascade into unrelated ones
    /// (finding 7 — the reviewer's mutation check hit exactly this, and got a PoisonError instead of
    /// the real assertion message).
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    struct TempCrashDir(PathBuf);
    impl TempCrashDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "eqoxide-crash-test-{}-{}-{}",
                tag,
                std::process::id(),
                now_epoch_secs()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::env::set_var(CRASH_DIR_ENV, &dir);
            Self(dir)
        }
        fn log_contents(&self) -> String {
            std::fs::read_to_string(crash_log_path()).unwrap_or_default()
        }
    }
    impl Drop for TempCrashDir {
        fn drop(&mut self) {
            std::env::remove_var(CRASH_DIR_ENV);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Reset the module's global state so a test exercises the fallback (open-per-line) path against
    /// its own temp dir rather than a leaked fd from another test.
    fn reset_globals() {
        CRASH_FD.store(-1, Ordering::Relaxed);
        BYTES_WRITTEN.store(0, Ordering::Relaxed);
        CAP_NOTICE_WRITTEN.store(false, Ordering::Relaxed);
        LOG_HEALTHY.store(true, Ordering::Relaxed);
    }

    #[test]
    fn paths_are_per_pid_so_concurrent_clients_do_not_share_a_log() {
        let _g = lock(&TEST_LOCK);
        let _d = TempCrashDir::new("paths");
        let p = crash_log_path();
        let h = heartbeat_path();
        let pid = std::process::id();
        assert!(
            p.to_string_lossy().contains(&pid.to_string()),
            "crash log must be per-pid or concurrent clients clobber each other: {}",
            p.display()
        );
        assert!(
            h.to_string_lossy().contains(&pid.to_string()),
            "heartbeat must be per-pid: {}",
            h.display()
        );
        assert_ne!(p, h);
    }

    #[test]
    fn every_record_type_carries_the_pid() {
        // Without a pid on every line, records from concurrent clients are unattributable and
        // "is the last line a clean exit?" cannot be answered (finding 4).
        let panic_line =
            format_panic_line(1_700_000_000, 4242, "nav-planner", "src/foo.rs:12:5", "boom");
        let exit_line = format_exit_line(1_700_000_000, 4242, "clean", 0);
        let inst_line = format_instance_line(1_700_000_000, 4242, "api_port=8901");
        for l in [&panic_line, &exit_line, &inst_line] {
            assert!(l.contains("pid=4242"), "record must carry the pid: {l}");
        }
        assert!(
            panic_line.contains("nav-planner")
                && panic_line.contains("src/foo.rs:12:5")
                && panic_line.contains("boom")
        );
        assert!(exit_line.contains("EXIT reason=clean"));
    }

    #[test]
    fn exit_reasons_distinguish_a_wedge_from_a_clean_shutdown() {
        // The whole point of finding 3: a watchdog-forced exit must not look like a clean one, or a
        // wedge gets misreported as an OOM-kill.
        let clean = format_exit_line(1, 1, "clean", 0);
        let wedged = format_exit_line(1, 1, "render-loop-wedged", 0);
        assert_ne!(clean, wedged);
        assert!(wedged.contains("render-loop-wedged"));
        assert!(!wedged.contains("reason=clean"));
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

    /// Finding 1 regression test: the first version of this module called
    /// `signal_hook::low_level::register(SIGSEGV, ..)`, which PANICS ("Attempted to register
    /// forbidden signal 11"), so the client could not start at all — even `--help` died with exit
    /// 101. `cargo test --lib` stayed green throughout, because NOTHING EVER CALLED THE INSTALL
    /// PATH. This test calls it.
    #[test]
    fn install_signal_handlers_actually_installs_and_does_not_panic() {
        let _g = lock(&TEST_LOCK);
        let n = install_signal_handlers();
        assert_eq!(
            n,
            FATAL_SIGNALS.len(),
            "every fatal signal must install; a forbidden/failed signal means ZERO coverage"
        );
    }

    /// Finding 2 regression test: `SA_ONSTACK` is what keeps a stack overflow loud. If a future
    /// change drops it (e.g. by switching to signal-hook's `register_signal_unchecked`, which never
    /// sets it), a stack overflow becomes a completely silent `exit 139` — this module would then be
    /// MANUFACTURING the bug it exists to prevent.
    #[test]
    fn installed_handlers_set_sa_onstack_and_keep_a_chain_to_the_previous_handler() {
        let _g = lock(&TEST_LOCK);
        install_signal_handlers();
        for sig in FATAL_SIGNALS {
            // SAFETY: querying the current disposition with a null `act` only reads it.
            let mut cur: libc::sigaction = unsafe { std::mem::zeroed() };
            let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut cur) };
            assert_eq!(rc, 0, "sigaction query failed for {}", signal_name(sig));
            assert!(
                cur.sa_flags & libc::SA_ONSTACK != 0,
                "{} MUST be installed with SA_ONSTACK or a stack overflow dies silently",
                signal_name(sig)
            );
            assert!(
                cur.sa_flags & libc::SA_SIGINFO != 0,
                "{} must use the SA_SIGINFO ABI so we can forward (sig, info, ctx) when chaining",
                signal_name(sig)
            );
            assert_eq!(
                cur.sa_sigaction,
                fatal_signal_handler as *const () as usize,
                "{} should point at our handler",
                signal_name(sig)
            );
            assert!(
                !PREV_ACTION[sig as usize].load(Ordering::Relaxed).is_null(),
                "{} must have captured a previous disposition to chain to",
                signal_name(sig)
            );
        }
    }

    /// A panic on a NON-main thread — the exact case #380 flags as dangerous — must land a record
    /// naming that thread in the durable log.
    ///
    /// MUTATION CHECK: with `append_line(&line)` removed from `install_panic_hook`, this goes RED.
    /// See the PR body for the actual observed failure output.
    #[test]
    fn panicking_worker_thread_lands_a_record_in_the_durable_crash_log() {
        let _g = lock(&TEST_LOCK);
        let d = TempCrashDir::new("panic");
        reset_globals();

        let previous = std::panic::take_hook();
        install_panic_hook();

        let handle = std::thread::Builder::new()
            .name("test-worker-thread".into())
            .spawn(|| panic!("synthetic panic for #380 verification"))
            .unwrap();
        // A panic on a non-main thread does NOT kill the process — `join` just returns Err. That
        // silent survival is precisely what #380 is about.
        assert!(handle.join().is_err(), "the worker thread should have panicked");

        std::panic::set_hook(previous);

        let contents = d.log_contents();
        assert!(
            contents.contains("test-worker-thread"),
            "crash log must name the panicking thread, got: {contents:?}"
        );
        assert!(
            contents.contains("synthetic panic for #380 verification"),
            "crash log must carry the panic message, got: {contents:?}"
        );
        assert!(contents.contains("PANIC"), "must be tagged PANIC, got: {contents:?}");
        assert!(
            contents.contains(&format!("pid={}", std::process::id())),
            "must carry the pid, got: {contents:?}"
        );
    }

    #[test]
    fn a_watchdog_exit_is_recorded_distinctly_from_a_clean_one() {
        let _g = lock(&TEST_LOCK);
        let d = TempCrashDir::new("exit");
        reset_globals();

        log_exit("render-loop-wedged", 0);

        let contents = d.log_contents();
        assert!(contents.contains("EXIT reason=render-loop-wedged"), "got: {contents:?}");
        assert!(!contents.contains("reason=clean"), "got: {contents:?}");
        assert!(!contents.contains("PANIC"), "got: {contents:?}");
    }

    #[test]
    fn clean_shutdown_writes_a_record_distinct_from_any_panic() {
        let _g = lock(&TEST_LOCK);
        let d = TempCrashDir::new("clean");
        reset_globals();

        log_clean_shutdown();

        let contents = d.log_contents();
        assert!(contents.contains("EXIT reason=clean"), "got: {contents:?}");
        assert!(!contents.contains("PANIC"), "got: {contents:?}");
    }

    /// Finding 5: a panicking tokio HTTP task an agent retries would otherwise append forever.
    #[test]
    fn the_log_is_size_capped_and_says_so_rather_than_growing_without_bound() {
        let _g = lock(&TEST_LOCK);
        let d = TempCrashDir::new("cap");
        reset_globals();

        let big = "x".repeat(4096);
        for _ in 0..((MAX_LOG_BYTES / 4096) + 8) {
            append_line(&big);
        }

        let len = std::fs::metadata(crash_log_path()).unwrap().len();
        assert!(len < MAX_LOG_BYTES + 8192, "log must stop near the cap, grew to {len} bytes");
        assert!(
            d.log_contents().contains("log capped"),
            "a truncated log must SAY it was truncated, or its silence is another lie"
        );
        reset_globals();
    }

    /// Finding 6: a failed durable write must be visible, not swallowed.
    #[test]
    fn a_failed_durable_write_marks_the_log_unhealthy() {
        let _g = lock(&TEST_LOCK);
        reset_globals();
        assert!(crash_log_healthy());

        // An fd that is open but not writable: /dev/null opened read-only.
        let ro = std::fs::File::open("/dev/null").unwrap();
        CRASH_FD.store(ro.as_raw_fd(), Ordering::Relaxed);

        append_line("this write must fail");

        assert!(
            !crash_log_healthy(),
            "a write failure must flip the health flag — otherwise 'no record' is \
             indistinguishable from 'we could not write one'"
        );
        reset_globals();
    }
}
