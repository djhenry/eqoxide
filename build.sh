#!/usr/bin/env bash
#
# build.sh — build eqoxide at reduced CPU/IO priority so a rebuild can't starve a
# running EQEmu server + eqoxide clients on the same box.
#
# Frequent `cargo build --release` rebuilds (agents pulling new commits) peg all
# cores; the load spike was high enough that the server's CLE subsystem dropped
# connected clients as linkdead — a "storm of rustc processes" took down a live
# group mid-play (eqoxide#151). This wrapper compiles under `nice`/`ionice` and
# leaves one core free, so the game processes keep the CPU/IO they need.
#
# Use it in place of `cargo build --release` whenever a server/clients are running:
#   ./build.sh                       # nice release build (default)
#   ./build.sh --bin render_model    # extra args pass straight through to cargo
#
# It degrades gracefully: if nice/ionice/nproc are missing it just runs cargo.
set -eo pipefail
cd "$(dirname "$0")"

# Leave one core free for the live game processes (portable — computed per machine,
# so we never hard-code a jobs count into the shared .cargo/config.toml).
JOBS=""
if command -v nproc >/dev/null 2>&1; then
  n=$(nproc)
  JOBS=$(( n > 1 ? n - 1 : 1 ))
fi

# Lowest CPU priority (nice 19) + idle I/O class (ionice -c3) when available.
PREFIX=()
command -v nice   >/dev/null 2>&1 && PREFIX+=(nice -n 19)
command -v ionice >/dev/null 2>&1 && PREFIX+=(ionice -c3)

set -x
exec "${PREFIX[@]}" cargo build --release ${JOBS:+-j "$JOBS"} "$@"
