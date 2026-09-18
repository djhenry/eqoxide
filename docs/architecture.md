# eqoxide — Architecture Overview

A standalone Rust EQEmu game client. Connects directly to a local EQEmu
server (login 127.0.0.1:5999), renders the zone in 3D via wgpu, and exposes a local HTTP API on
the next free port from 8765 (printed to stdout as `API_PORT=<port>`, so multiple instances can
run at once — see `http-api.md`) for agent scripting. It can log in either as a **GM observer** (the original use case)
or as a **regular non-GM player character** that actually plays — fights, levels, travels, buys
(see `autonomous-play.md`). The account/character is set in the login config; the renderer and API
are identical either way.

Optionally, `--agent-socket <PATH>` also binds a low-latency Unix-socket API purpose-built for a
tight tick-driven control loop (e.g. an RL policy), alongside — not instead of — the HTTP API. It's
off by default; see `agent-api.md`.

---

## Thread Model

```
main thread          eq_net thread            HTTP thread              agent-plugin-host thread
─────────────        ─────────────────        ───────────────────     ───────────────────────
winit event loop     login.rs state machine   axum server (next        (opt-in: --agent-socket)
wgpu rendering       packet_handler.rs        free port from 8765)     tokio server on a Unix
hud.rs (egui)        navigation.rs tick       reads/writes shared      domain socket; same
app.rs WASD          gameplay.rs zone change  Arcs: GotoTarget,        shared Arcs, one
                                               HailReq, SayReq,         connection at a time
                                               TargetReq,
                                               EntityPositions,
                                               ZonePoints, FrameReq
```

State flows one-way: `eq_net → GameState → SceneState → render`. The HTTP thread and the
agent-plugin-host thread are two independent front doors onto the same shared-Arc request slots —
neither depends on the other, and both can be in use at once.

---

## Key Shared Types (crate `eqoxide-ipc`, HTTP routes in crate `eqoxide-http`)

| Type               | Direction        | Purpose |
|--------------------|-----------------|---------|
| `FrameReq`         | HTTP→render      | Capture a PNG frame |
| `GotoTarget`       | HTTP→nav         | Walk to (x,y,z) |
| `EntityPositions`  | nav→HTTP         | All entity name→pos |
| `ZonePoints`       | nav→HTTP         | Zone exit locations |
| `ZoneCrossReq`     | HTTP→nav         | Trigger OP_ZONE_CHANGE |
| `HailReq`          | HTTP/HUD→nav     | Say "Hail, <name>" |
| `SayReq`           | HTTP/HUD→nav     | Say arbitrary text |
| `TargetReq`        | HTTP/HUD→nav     | Set target + consider |
| `SharedCollision`  | render→nav       | Spatial grid for movement |

All types are `Arc<Mutex<Option<…>>>` or similar; both producer and consumer take
`.lock().unwrap()` then `.take()` (one-shot) or `.clone()` (shared read). `SharedCollision` lives in
`eqoxide-nav`; the rest are defined in `eqoxide-ipc` and re-exported by the HTTP routes in
`eqoxide-http`. The Agent Plugin API (`eqoxide-agent-plugin-host`) writes to these same slots
directly rather than through its own parallel set — see `agent-api.md`.

---

## Coordinate System

EQ server coordinates and GPU world space are **swapped** — this is a persistent
source of confusion:

```
server_x  =  north/south   = GPU world [1] (north axis)
server_y  =  east/west     = GPU world [0] (east axis)
server_z  =  height        = GPU world [2]
```

Map coordinates (used in the minimap and zone map files):

```
map_x  = server_y  (east)
map_y  = server_x  (north)
```

So `POST /v1/move/goto {"map_x": 150, "map_y": 200}` translates to `server_x=200, server_y=150`.

libeq_wld mesh positions are `[east, height, north]` — note height is the middle
element, not the last. Collision code converts to `[east, north, height]` for GPU world space.

---

## Crate Map

The client was originally one monolithic `src/*.rs` binary crate. It has since been split
(issue #544) into a Cargo workspace of library crates, with the root `eqoxide` binary/lib crate
(`src/lib.rs`) re-exporting most of them under their old module names (`pub use eqoxide_net as
eq_net;` etc.) so existing `crate::foo::…` call sites kept resolving unchanged. `src/` itself now
holds only the winit/wgpu app glue that was never worth extracting: `main.rs` (entry point; wires
shared arcs; runs the event loop), `app.rs` (`ApplicationHandler`; WASD input; ground-snap; camera),
`hud.rs` (egui panels), `movement.rs`, `zone_in.rs`, `asset_sync.rs`, `debug_zone.rs`,
`camera_state.rs`, `model.rs`, `logging.rs`, `profiling.rs`.

| Crate | Role |
|-------|------|
| `eqoxide-core` | Dependency-free leaf modules: `game_state` (`GameState`, authoritative state updated by the eq_net thread), `zone_map` (`.txt` 2D map loader), `eqstr` (`eqstr_us.txt` string table), `coord`, `config`, `charname`, `region_map`, `skills`, `spells` |
| `eqoxide-ipc` | Inter-thread contracts: the shared-Arc request-slot types (`GotoTarget`, `HailReq`, `SayReq`, `TargetReq`, `EntityPositions`, `ZonePoints`, `FrameReq`, …) |
| `eqoxide-protocol` | RoF2 wire-format decode layer; opcode constants; position decode/encode (bit-packed) |
| `eqoxide-net` | EQ network client: transport (UDP session, CRC/XOR/compression, fragment reassembly), login→world→zone state machine, `packet_handler` (dispatch inbound opcodes → `GameState` mutations), `navigation` (`Navigator::tick()`; hail/say/target/goto; wall-sliding), zone-change reconnect flow — re-exported as `eq_net` |
| `eqoxide-nav` | Navigation domain: `collision` (`Collision::build()` spatial grid, `SharedCollision`), `traversability` |
| `eqoxide-assets` | S3D zone + texture asset loading (`ZoneAssets::load()`) |
| `eqoxide-command` | `CommandState` — the write-path IPC facade the HTTP and Agent Plugin APIs both dispatch through |
| `eqoxide-renderer` | wgpu frame, render passes, models, camera, scene (`SceneState` — renderer's view of game state, cloned each frame), billboard, animation |
| `eqoxide-ui` | egui window system |
| `eqoxide-http` | The HTTP/REST API (axum; next free port from 8765) — see `http-api.md` |
| `eqoxide-agent-protocol` | Wire types for the Agent Plugin API (`Step`, `Observation`, handshake, NDJSON framing); zero dependency on any other eqoxide crate — see `agent-api.md` |
| `eqoxide-agent-vision-filter` | Client-side visibility filtering (distance cutoff + line-of-sight occlusion) feeding `Observation.visible` |
| `eqoxide-agent-plugin-host` | In-client server for the Agent Plugin API: Unix socket accept loop, handshake, per-tick `Step`/`Observation` — opt-in via `--agent-socket <PATH>` |
| `eqoxide-crash` | Crash/shutdown observability |
| `eqoxide-telemetry` | Default-off packet telemetry capture rig |
| `tools` | Standalone dev tooling, outside the `eqoxide` binary |

`dev-run.sh` watches the binary and auto-relaunches the client on rebuild.

---

## Zone Loading Sequence

1. `OP_NEW_ZONE` → `eqoxide-net`'s `packet_handler` sets `gs.zone_name`
2. `src/app.rs` detects `scene.zone_changed`, starts async asset load from `.s3d`
3. `eqoxide-assets`'s `ZoneAssets::load()` → `eqoxide-nav`'s `Collision::build(assets, 32.0)` → stored in `SharedCollision`
4. `SharedCollision` published to nav thread (movement collision) and render thread (label occlusion)
5. `eqoxide-core`'s `ZoneMap::load()` merges `_1/_2/_3.txt` layers → minimap overlay

---

## Player Profile Struct Offsets (Titanium)

`eqoxide-net`'s `packet_handler::parse_player_profile` reads `OP_PLAYER_PROFILE` (opcode `0x75df`):

| Field | Byte offset | Type |
|-------|------------|------|
| class | 12 | u8 |
| level | 20 | u8 |
| STR   | 2236 | u32 |
| STA   | 2240 | u32 |
| CHA   | 2244 | u32 |
| DEX   | 2248 | u32 |
| INT   | 2252 | u32 |
| AGI   | 2256 | u32 |
| WIS   | 2260 | u32 |
| platinum | 4428 | u32 |
| gold     | 4432 | u32 |
| silver   | 4436 | u32 |
| copper   | 4440 | u32 |
