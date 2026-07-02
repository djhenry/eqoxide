#!/usr/bin/env bash
#
# dev-run.sh — run the eqoxide renderer and auto-restart it whenever the
# compiled binary changes (i.e. whenever a new `cargo build` lands).
#
# Run this in your own terminal (or let the agent launch it via Bash) to keep the
# client alive across rebuilds. After each `cargo build --release`, this script
# detects the new binary and relaunches automatically. The agent can also launch
# eqoxide directly without this script when auto-restart is not needed.
#
# Build POLITELY while a server/clients are running: use ./build.sh instead of a bare
# `cargo build --release`. It compiles under nice/ionice and leaves a core free so a
# rebuild storm can't spike load and trip the server's CLE linkdead drop (#151).
#
# Usage:
#   ./dev-run.sh              # release build (default): target/release/eqoxide
#   ./dev-run.sh debug        # debug build:             target/debug/eqoxide
#   BIN=/path/to/eqoxide ./dev-run.sh
#
# Env (auto-defaulted if unset): DISPLAY, WAYLAND_DISPLAY, XDG_RUNTIME_DIR, RUST_LOG
# Log: /tmp/eqoxide.log  (the agent reads this)

set -u

# ── Config ────────────────────────────────────────────────────────────────────
PROFILE="${1:-release}"
REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BIN="${BIN:-$REPO_DIR/target/$PROFILE/eqoxide}"
LOG="${LOG:-/tmp/eqoxide.log}"
POLL_SECS="${POLL_SECS:-2}"

# Display/session env — only set defaults if the caller hasn't.
export DISPLAY="${DISPLAY:-:0}"
export WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-wayland-0}"
export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export RUST_LOG="${RUST_LOG:-warn}"

CLIENT_PID=""

# ── Helpers ───────────────────────────────────────────────────────────────────

# A signature that changes when the binary is rebuilt (mtime + size, portable).
sig() {
  stat -c '%Y-%s' "$BIN" 2>/dev/null || echo "missing"
}

log() { printf '\033[36m[dev-run]\033[0m %s\n' "$*"; }

stop_client() {
  if [[ -n "$CLIENT_PID" ]] && kill -0 "$CLIENT_PID" 2>/dev/null; then
    log "stopping client (pid $CLIENT_PID)"
    kill "$CLIENT_PID" 2>/dev/null
    # give it a moment, then force
    for _ in 1 2 3 4 5; do
      kill -0 "$CLIENT_PID" 2>/dev/null || break
      sleep 0.3
    done
    kill -9 "$CLIENT_PID" 2>/dev/null || true
  fi
  CLIENT_PID=""
}

start_client() {
  log "launching $BIN (log: $LOG)"
  : > "$LOG"                       # truncate so the agent sees a fresh boot
  "$BIN" >>"$LOG" 2>&1 &
  CLIENT_PID=$!
  log "client pid $CLIENT_PID — HTTP API at http://localhost:8765"
}

cleanup() {
  log "shutting down"
  stop_client
  exit 0
}
trap cleanup INT TERM EXIT

# ── Main ──────────────────────────────────────────────────────────────────────

log "repo:    $REPO_DIR"
log "binary:  $BIN ($PROFILE)"
log "watching for rebuilds every ${POLL_SECS}s — Ctrl-C to quit"

# Wait for the binary to exist on first run.
while [[ "$(sig)" == "missing" ]]; do
  log "binary not found yet — build it (cargo build ${PROFILE:+--$PROFILE} ... ) — waiting"
  sleep "$POLL_SECS"
done

RUNNING_SIG="$(sig)"
start_client
PREV_SIG="$RUNNING_SIG"

while true; do
  sleep "$POLL_SECS"
  CUR_SIG="$(sig)"

  # Restart only when the binary changed AND has settled (same for one full poll),
  # so we don't relaunch a half-written binary mid-`cargo build`.
  if [[ "$CUR_SIG" != "missing" && "$CUR_SIG" != "$RUNNING_SIG" && "$CUR_SIG" == "$PREV_SIG" ]]; then
    log "new build detected — restarting"
    stop_client
    sleep 0.5
    RUNNING_SIG="$CUR_SIG"
    start_client
  fi

  # Client died on its own (crash / clean exit) — relaunch it.
  if [[ -n "$CLIENT_PID" ]] && ! kill -0 "$CLIENT_PID" 2>/dev/null; then
    log "client exited (see $LOG) — relaunching"
    RUNNING_SIG="$(sig)"
    start_client
  fi

  PREV_SIG="$CUR_SIG"
done
