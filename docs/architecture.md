# eqoxide — Architecture Overview

A standalone Rust EverQuest Titanium observer/renderer. Connects directly to a local EQEmu
server (login 127.0.0.1:5998), renders the zone in 3D via wgpu, and exposes a local HTTP API on
the next free port from 8765 (printed to stdout as `API_PORT=<port>`, so multiple instances can
run at once — see `http-api.md`) for agent scripting. It can log in either as a **GM observer** (the original use case)
or as a **regular non-GM player character** that actually plays — fights, levels, travels, buys
(see `autonomous-play.md`). The account/character is set in the login config; the renderer and API
are identical either way.

---

## Thread Model

```
main thread          eq_net thread            HTTP thread
─────────────        ─────────────────        ───────────────────
winit event loop     login.rs state machine   axum server (next free port from 8765)
wgpu rendering       packet_handler.rs        reads/writes shared Arcs:
hud.rs (egui)        navigation.rs tick         GotoTarget, HailReq,
app.rs WASD          gameplay.rs zone change     SayReq, TargetReq,
                                                 EntityPositions,
                                                 ZonePoints, FrameReq
```

State flows one-way: `eq_net → GameState → SceneState → render`.

---

## Key Shared Types (src/http.rs)

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
`.lock().unwrap()` then `.take()` (one-shot) or `.clone()` (shared read).

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

So `POST /v1/navigate/goto {"map_x": 150, "map_y": 200}` translates to `server_x=200, server_y=150`.

libeq_wld mesh positions are `[east, height, north]` — note height is the middle
element, not the last. Collision code converts to `[east, north, height]` for GPU world space.

---

## File Map

| File | Role |
|------|------|
| `src/main.rs` | Entry point; wires shared arcs; runs event loop |
| `src/app.rs` | winit `ApplicationHandler`; WASD input; ground-snap; camera |
| `src/renderer.rs` | wgpu frame; calls hud, billboard, zone render passes |
| `src/scene.rs` | `SceneState` — renderer's view of game state (cloned each frame) |
| `src/game_state.rs` | `GameState` — authoritative state; updated by eq_net thread |
| `src/hud.rs` | egui HUD panels: status bar, NPC dialogue, controls, minimap, labels |
| `src/http.rs` | HTTP API server (axum, port 8765); all shared-arc type aliases |
| `src/assets.rs` | S3D zone loading; `Collision` spatial grid; `SharedCollision` |
| `src/models.rs` | Character/NPC model loading; race→archetype→scale mapping |
| `src/zone_map.rs` | `.txt` 2D map line loader (minimap overlay) |
| `src/eqstr.rs` | `eqstr_us.txt` string table for OP_FormattedMessage |
| `src/eq_net/transport.rs` | UDP EQ session; CRC/XOR/compression; fragment reassembly |
| `src/eq_net/login.rs` | Login→World→Zone state machine |
| `src/eq_net/packet_handler.rs` | Dispatch all inbound opcodes → `GameState` mutations |
| `src/eq_net/navigation.rs` | `Navigator::tick()`; hail/say/target/goto; wall-sliding |
| `src/eq_net/protocol.rs` | All opcode constants; position decode/encode (bit-packed) |
| `src/eq_net/gameplay.rs` | Zone-change reconnect flow |
| `dev-run.sh` | Watches binary; auto-relaunches client on rebuild |

---

## Zone Loading Sequence

1. `OP_NEW_ZONE` → `packet_handler` sets `gs.zone_name`
2. `app.rs` detects `scene.zone_changed`, starts async asset load from `.s3d`
3. `ZoneAssets::load()` → `Collision::build(assets, 32.0)` → stored in `SharedCollision`
4. `SharedCollision` published to nav thread (movement collision) and render thread (label occlusion)
5. `ZoneMap::load()` merges `_1/_2/_3.txt` layers → minimap overlay

---

## Player Profile Struct Offsets (Titanium)

`parse_player_profile` reads `OP_PLAYER_PROFILE` (opcode `0x75df`):

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
