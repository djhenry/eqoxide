# Development Workflow

---

## Running the Client

The harness (Claude Code / CI) reaps GUI child processes started via `Bash` tool,
so **never** launch the renderer from a Bash tool call. Instead, run `dev-run.sh`
in a separate user terminal.

```sh
# Release build (default):
./dev-run.sh

# Debug build:
./dev-run.sh debug

# Override binary path:
BIN=/path/to/eqoxide ./dev-run.sh
```

`dev-run.sh` watches `target/release/eqoxide` (mtime + size). When a new
build is detected and has settled (same signature for two polls), it kills the old
client and starts the new one automatically. It also relaunches on crash.

### Choosing the character (`--config`)

The login config (which account/character to log in as) defaults to
`~/.config/eqoxide/config.yaml`, but you can point the client at any per-character config with
`--config`. The argument may be a profile name, a bare filename (resolved under
`~/.config/eqoxide/`), or an explicit path:

```sh
./target/debug/eqoxide --config durgan      # ~/.config/eqoxide/config-durgan.yaml
./target/debug/eqoxide --config claude
```

Per-character config files (`config-durgan.yaml`, `config-claude.yaml`, `config-aiquestbot.yaml`)
live in `~/.config/eqoxide/` and carry just the `server:` + `account:` blocks. They are **not**
checked into source control (they hold credentials). This lets multiple worktrees/agents each run
their own character from a shared, per-user config dir (pairs with the auto-port + `/v1/lifecycle/exit`
multi-instance support — see `http-api.md`).

**To fully stop / log the character out** (e.g. to keep the client down, or to edit
`character_data` position in the DB without it being clobbered), you must kill the **dev-run
watcher** too — otherwise it auto-relaunches the renderer within seconds. Find it with
`pgrep -af dev-run.sh | grep -v 'bash -c'` and kill that PID, then `pkill -x eqoxide`; confirm
with `ps`. Relaunching the client soon after resumes the *linkdead* zone session at the old
position — wait > ~90s (past `Zone:ClientLinkdeadMS`) before a DB position edit will stick. See
`autonomous-play.md` §6.

Logs: `/tmp/eqoxide.log` (truncated on each launch).

### Frame profiling (`--profile`)

Pass `--profile` (or set `EQ_PROFILE=1`) to turn on a lightweight per-phase frame-timing overlay,
drawn top-left under the fps counter:

```sh
./target/debug/eqoxide --testzone --profile
EQ_PROFILE=1 ./dev-run.sh debug
```

The overlay shows smoothed milliseconds for each phase of `render_frame` — `update` (packet drain,
physics, camera, scene build), `render` (3D pass), `egui`, `submit` — plus `cpu` (total CPU-side cost)
and `frame` (wall-clock interval between rendered frames). It costs nothing when the flag is off, and
needs no external profiler.

Note: because the render loop is event-driven (see `architecture.md`), a still scene stops rendering,
so the `frame` time and fps legitimately read low / "spiky" when idle — that is the idle-CPU saving at
work, not a stall. The numbers climb to a steady ~16 ms only while something is actually moving.

---

## Multiple Instances / API Port Discovery

The client binds the **next free** HTTP API port starting at `config.yaml` `http_port`
(default 8765), scanning upward (8765, 8766, 8767…). This lets several instances — e.g.
one per git worktree, for working on multiple features at once — run side by side without
colliding. **Do not assume port 8765.** On launch the client prints the port it bound to
**stdout** (also captured in `/tmp/eqoxide.log`):

```
API_PORT=8766
```

Capture it instead of hardcoding:

```sh
PORT=$(grep -m1 -oP 'API_PORT=\K[0-9]+' /tmp/eqoxide.log)
curl "http://127.0.0.1:$PORT/v1/observe/debug"
```

**To restart your own instance, use `POST /v1/lifecycle/exit`** — never `pkill eqoxide`, which would
also kill another worktree's client. It cleanly exits only the instance on the port you call:

```sh
curl -X POST "http://127.0.0.1:$PORT/v1/lifecycle/exit"
```

See `http-api.md` for both the port-discovery convention and the `/v1/lifecycle/exit` endpoint.

---

## Build + Verify Loop

1. Make a code change
2. `cargo build --release` (or `cargo test` first)
3. `dev-run.sh` detects the new binary and restarts the client within ~4 seconds
4. `curl "http://127.0.0.1:$PORT/v1/observe/frame" -o /tmp/frame.png` to capture the current screen
   (`PORT` from the `API_PORT=` line — see *API Port Discovery* above; single-instance dev
   defaults to 8765)
5. Read `/tmp/frame.png` with the image viewer or the `Read` tool to inspect visually

---

## Checking the Log

```sh
tail -50 /tmp/eqoxide.log
```

Logging goes through the `tracing` framework. Verbosity is set by an env filter, in precedence order
`EQ_LOG` → `RUST_LOG` → default `info`. `dev-run.sh` exports `RUST_LOG=warn`, so the log is at `warn`
by default under it. For more detail:
```sh
RUST_LOG=debug ./dev-run.sh
# or, client-only without affecting other RUST_LOG-aware tools:
EQ_LOG=debug ./dev-run.sh
# per-subsystem, e.g. chatty network but quiet elsewhere:
EQ_LOG=info,eqoxide::eq_net=debug ./dev-run.sh
```

Key log lines to watch:
- `API_PORT=<port>` — the HTTP API port this instance bound (may not be 8765; see *API Port Discovery*)
- `camera HTTP: http://127.0.0.1:<port>` — server is up
- `eqstr: loaded N strings` — string table loaded
- `NAV: arrived at (…)` — /v1/navigate/goto completed
- `EQ: hailing '…'` — hail packet sent
- `EQ: say: …` — say packet sent
- `WARN … unhandled opcode` — new packet type to handle

---

## Testing

```sh
cargo test                    # all tests
cargo test --lib              # library tests only (no integration)
cargo test collision          # tests matching "collision"
cargo test -- --nocapture     # see println! output
```

Tests that require real zone assets are marked `#[ignore]`:
```sh
cargo test -- --ignored       # run asset-dependent tests (needs ~/eq_assets)
```

Key test modules:
- `src/assets.rs` — collision grid (floor_z, segment_blocked, path_clear)
- `src/eq_net/navigation.rs` — slide_move, packet builders (say, target, consider)
- `src/hud.rs` — split_keywords, nearest_npc_name
- `src/eqstr.rs` — string table parse + substitute

---

## Config

`config.yaml` in the repo root:

```yaml
login_host: "127.0.0.1"
login_port: 5998
assets_path: "~/eq_assets/EQ_Files"
models_path: "assets/models"
character_name: "Aiquestbot"
```

`assets_path` must contain the EQ `.s3d` zone files. `models_path` contains
GLTF character models organized as `assets/models/<archetype>/<archetype>.glb`.

---

## Archetype → Model Scale

Models are loaded from `assets/models/<archetype>/`. The scale factors in
`src/models.rs: archetype_scale()` control how large each model appears relative
to zone geometry. Current calibrated values (as of 2026-06-15):

| Archetype | Scale | Notes |
|-----------|-------|-------|
| humanoid | 3.5 | Player/humanoid NPC — calibrated to doorway height |
| skeleton | 2.5 | |
| wolf | 2.0 | |
| goblin | 2.0 | |

If the model looks too tall or small relative to buildings/doorways, halve or
double the scale for that archetype and rebuild.

---

## Adding New Packet Handlers

1. Find the opcode in EQEmu's [`utils/patches/patch_Titanium.conf`](https://github.com/EQEmu/Server/blob/master/utils/patches/patch_Titanium.conf) — search for
   the packet name (e.g. `OP_WearChange`) to get the correct hex value.
2. Check the struct layout in EQEmu's [`common/patches/titanium_structs.h`](https://github.com/EQEmu/Server/blob/master/common/patches/titanium_structs.h).
3. Add the opcode constant to `src/eq_net/protocol.rs`.
4. Add a match arm in `src/eq_net/packet_handler.rs: handle_packet()`.
5. Mutate `GameState` appropriately; mirror to `SceneState` if needed for rendering.
6. If the handler reads a fixed-size struct, add a `len < SIZE` guard **before**
   indexing — and verify the size against the actual Titanium struct, not a guess.

**Common pitfall**: wrong struct size in the guard. If a handler silently never fires,
check that the size constant matches the Titanium struct (not a different EQ version).
Example: `SIZE_SPAWN_POSITION_UPDATE` was 30 (wrong) — should be 22. Every NPC
position update was silently dropped.

---

## Autonomous Agent Cron Setup

To schedule recurring autonomous development runs:

```sh
# Create an hourly cron (via Claude Code /schedule skill)
# Stop it with:
/schedule list   # find the cron id
/schedule delete <id>
```

The `autonomous-run-credit-guard.md` memory file documents the credit threshold
at which autonomous runs should stop (currently ~5%).
