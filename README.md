# eq_client_lite

A standalone Rust EverQuest **Titanium** observer/renderer. It connects directly to a local
EQEmu server, renders the zone in 3D (wgpu), and exposes a local **HTTP API** so an agent (or
any script) can drive the client — move, hail NPCs, target, fight, buy, capture frames, and more.
It can log in as a **GM observer** or as a **regular player character** that actually plays
(see [`docs/autonomous-play.md`](docs/autonomous-play.md)).

## Build & Run

```sh
cargo build --release
./dev-run.sh           # release build; auto-restarts on rebuild/crash
./dev-run.sh debug     # debug build
```

Run `dev-run.sh` in your **own terminal**, not from an agent/Bash tool call — the harness reaps
GUI child processes. Logs go to `/tmp/eq_client.log`. Per-character server/credentials and
renderer asset paths live in `~/.config/eqoxide/` (honoring `XDG_CONFIG_HOME`). Credential files
are kept out of source control — copy a template into that directory and edit it locally.

Offline asset/zone debugging (no server): `./target/release/eq_renderer --testzone`.

Add `--profile` (or `EQ_PROFILE=1`) for a per-phase frame-timing overlay; see `docs/dev-workflow.md`.

### Choosing which character logs in

The account + character to log in as is **not** a CLI name argument — it comes from the login
config file. Login configs live in `~/.config/eqoxide/`. Pass one with `--config`:

```sh
./target/release/eq_renderer --config durgan   # ~/.config/eqoxide/config-durgan.yaml
```

`--config` accepts:
- a **profile name** (`durgan`) → `~/.config/eqoxide/config-durgan.yaml`
- a **bare filename** (`config-durgan.yaml`) → looked up in `~/.config/eqoxide/`
- an explicit **path** (`./foo.yaml`, `~/elsewhere/x.yaml`) → used as-is

With no `--config`, the client loads `~/.config/eqoxide/config.yaml`. Each `config-<name>.yaml`
sets its own `account.username`, `account.password`, and `account.character_name`. To add a
character, copy an existing file in `~/.config/eqoxide/`, edit those three fields (the
account/character must already exist on the EQEmu server), and pass its name to `--config`.

```sh
ls ~/.config/eqoxide/config-*.yaml          # available login profiles
grep character_name ~/.config/eqoxide/config-*.yaml
```

### Launching from inside an agent harness (no interactive terminal)

`dev-run.sh` assumes its own terminal. If you must launch from a Bash tool call (where the harness
reaps GUI children), detach with `setsid` so the process survives, then read the printed
`API_PORT=` line:

```sh
setsid bash -c 'XDG_RUNTIME_DIR=/run/user/$(id -u) DISPLAY=:0 WAYLAND_DISPLAY=wayland-0 \
  exec ./target/release/eq_renderer --config durgan' \
  > /tmp/eq_durgan.log 2>&1 < /dev/null &
disown
sleep 12
PORT=$(grep -m1 -oP 'API_PORT=\K[0-9]+' /tmp/eq_durgan.log)   # do not hardcode 8765
grep -E "entering world as|sent ReqClientSpawn" /tmp/eq_durgan.log   # confirm zone-in
```

Requires the local EQEmu server (login `127.0.0.1:5998`) and a running X/Wayland session on
display `:0`.

## Running multiple instances at once

The client supports **several instances side by side** — e.g. one per git worktree, so multiple
agents can work on different features simultaneously without interfering.

- **Auto-port binding.** Each instance binds the **next free** HTTP API port starting at
  `config.yaml` `http_port` (default **8765**), scanning upward: 8765, 8766, 8767, …
- **Port is printed to stdout.** On launch the client prints a single parseable line (also in
  `/tmp/eq_client.log`). **Always read this — do not hardcode 8765:**

  ```
  API_PORT=8766
  ```

  ```sh
  PORT=$(grep -m1 -oP 'API_PORT=\K[0-9]+' /tmp/eq_client.log)
  curl "http://127.0.0.1:$PORT/debug"
  ```

- **Shut down your own instance with `POST /exit`** — never `pkill eq_renderer`, which could
  kill another worktree's client. `/exit` cleanly stops only the instance on the port you call:

  ```sh
  curl -X POST "http://127.0.0.1:$PORT/exit"     # 200 "shutting down", then the process exits
  ```

See [`docs/http-api.md`](docs/http-api.md) for the full endpoint reference and
[`docs/dev-workflow.md`](docs/dev-workflow.md) for the build/verify loop.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) — thread model, shared types, data flow
- [`docs/http-api.md`](docs/http-api.md) — REST API reference (port discovery, `/exit`, all endpoints)
- [`docs/dev-workflow.md`](docs/dev-workflow.md) — building, running, multi-instance, verify loop
- [`docs/autonomous-play.md`](docs/autonomous-play.md) — playing as a real character
- [`docs/protocol-notes.md`](docs/protocol-notes.md) — EQ/Titanium wire protocol notes
- [`docs/collision-system.md`](docs/collision-system.md), [`docs/zone-rendering.md`](docs/zone-rendering.md),
  [`docs/character-models.md`](docs/character-models.md) — rendering internals
