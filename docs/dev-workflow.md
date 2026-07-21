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
live in `~/.config/eqoxide/` and usually carry just the `server:` + `account:` blocks — but they
may also carry a `renderer:` block, which overrides the global `config.yaml` key by key (see
[Config](#config) below for the precedence rule and the startup disclosure). They are **not**
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
2. `./build.sh` (or `cargo build --release`; run `cargo test` first if useful)
3. `dev-run.sh` detects the new binary and restarts the client within ~4 seconds
4. `curl "http://127.0.0.1:$PORT/v1/observe/frame" -o /tmp/frame.png` to capture the current screen
   (`PORT` from the `API_PORT=` line — see *API Port Discovery* above; single-instance dev
   defaults to 8765)
5. Read `/tmp/frame.png` with the image viewer or the `Read` tool to inspect visually

### Build politely when the game is running (`./build.sh`)

The EQEmu server, its zones, and any eqoxide clients share this box with your rebuilds. A full
`cargo build --release` pegs every core, and the load spike can be high enough that the server's
CLE subsystem drops connected clients as **linkdead** — a storm of `rustc` processes has taken
down a live group mid-play (#151).

**`./build.sh`** wraps `cargo build --release` in `nice -n 19 ionice -c3` and leaves one core
free (`-j $(nproc)-1`), so a rebuild yields CPU/IO to the live game instead of starving it. Prefer
it (or `nice -n 19 ionice -c3 cargo build --release`) any time a server/clients are running —
this is the recommended default for automation and agents. It falls back to a plain build if
`nice`/`ionice` aren't available, and passes extra args straight through (`./build.sh --bin render_model`).

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
- `NAV: arrived at (…)` — /v1/move/goto completed
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
- `src/eq_net/navigation.rs` — the nav walker/planner loop, packet builders (say, target, consider)
- `src/hud.rs` — split_keywords, nearest_npc_name
- `src/eqstr.rs` — string table parse + substitute

---

## Config

The global config is `~/.config/eqoxide/config.yaml` (falling back to `./config.yaml`
in the working directory for back-compat):

```yaml
renderer:
  assets_path: ~/eq_assets/EQ_Files      # EQ .s3d zone files
  models_path: assets/models             # GLTF models: <archetype>/<archetype>.glb
  asset_server_url: http://localhost:8088
  eq_ui_dir: ~/eq_client/uifiles/default   # optional (#162 icon atlases)
http_port: 8765                          # BASE port for the agent HTTP API (top level!)

server:                                  # login settings
  login_host: 127.0.0.1
  login_port: 5999
  world_port: 9000
account:
  username: ...
  password: ...
  character_name: Aiquestbot
```

**The `./config.yaml` fallback above applies only to the renderer/HTTP settings**
(`renderer:` and `http_port`, loaded by `AppConfig`). The login settings (`server:` /
`account:`, loaded by `LoginConfig`) have **no such fallback** — with no `--config` flag
they are read only from `~/.config/eqoxide/config.yaml`, never from a `./config.yaml` in
the working directory. So a `config.yaml` that exists only in the cwd silently supplies
renderer settings and *no* login settings: the client falls back to its built-in login
defaults (`127.0.0.1:5999`, empty credentials) with no warning, which looks like a login
failure rather than a config-location mistake. Put the file in `~/.config/eqoxide/` (or
pass `--config`) if it needs to carry `server:`/`account:`.

### Precedence: per-character config overrides the global one, key by key (#597)

`--config <name|path>` selects a per-character file — and it selects **both** the login
settings *and* the renderer/HTTP settings. The two files are merged into one effective
config, **key by key**, with the per-character file winning:

1. `~/.config/eqoxide/config.yaml` (or `./config.yaml`) — the base layer;
2. the file `--config` resolved to — layered on top, when it is a *different* file.

A per-character file that sets only `renderer.asset_server_url` therefore still inherits
`assets_path`, `models_path`, `eq_ui_dir` and `http_port` from the global file. With no
`--config` there is only layer (1), which is exactly the historical behavior.

Before #597 the renderer block of a per-character file was read by nobody: the client
accepted the config, reported no error, and talked to whatever asset server the *global*
file named. The symptom was a world with no geometry and no collision, with nothing
saying the configured URL had been discarded.

Two guardrails keep that from recurring:

- **Startup disclosure.** The client logs the effective `asset_server_url`, `http_port`,
  `assets_path`, `models_path` and `eq_ui_dir` together with the file each one came from:
  `config: effective asset_server_url=http://prod-assets:8088 (from ~/.config/eqoxide/config-x.yaml)`.
  A wrong value is readable in the log rather than inferred later from an empty world.
- **No silent drops.** Every way a renderer setting can fail to take effect warns by key,
  file and reason instead of vanishing:
  - an unknown key under `renderer:` (`unknown key 'renderer.asset_serve_url' is IGNORED`);
  - `http_port` nested under `renderer:` — it is a **top-level** key;
  - a renderer key at the **top level** — it belongs under `renderer:`. (Older docs showed
    that layout, so configs written against them hit this.)
  - a key that is present but unusable: a non-string path/URL (`asset_server_url:` with no
    value), a non-integer or out-of-range `http_port`. Such a value is **not** treated as a
    hit — the previous layer or the built-in default stands, and the disclosure attributes
    the value to the file that actually contains it.
  - `renderer:` itself being the wrong shape, e.g. a list (`renderer: [1, 2]`) instead of a
    map. (`renderer:` with no value, and `renderer: {}`, are legitimate no-ops and stay
    silent — there is nothing to warn about.)

  An explicitly empty string (`asset_server_url: ""`) *is* a value: it overrides and is
  disclosed as such.

One caveat the disclosure states inline: `eq_ui_dir` has a consumer that can override it —
`eqoxide-ui`'s icon loader prefers `$EQ_UI_DIR` (and `$EQ_SPELL_ICONS_DIR` when nothing else
is set) and falls back to a default atlas dir when all are unset. The `eq_ui_dir` disclosure
line says which of those is in force; the dir finally chosen is logged as `ui icons: using
atlas dir …`. The other four keys have no such override.

`http_port` remains only the *base* port: the HTTP server scans upward from it for a free
port so several clients can run at once, and prints the port it actually bound as
`API_PORT=<n>`. `--api-port N` still overrides everything and binds exactly N.

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
