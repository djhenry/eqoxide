# Plan: Standalone EQEmu GM Observer Client

## Context

The `eq_renderer` (Rust/wgpu 3D viewer) currently runs as a standalone desktop app that
receives game state from a Python bot via Unix domain socket. The goal is to eliminate the
Unix socket IPC and the Python bot entirely, converting this crate into a single standalone
binary that:

1. Connects directly to an EQEmu server as a GM-level character
2. Uses GM commands (`#goto`, `#set hide_me on`) to follow a target player
3. Receives real-time entity position updates (`OP_CLIENT_UPDATE`) from the zone server
4. Renders the scene in real-time via the existing wgpu renderer

A separate Python EQ client (the `eq_client` package) is the reference implementation for
the protocol layer. Everything we need is already implemented there.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                   EQEmu Server (unchanged)                    │
│                                                              │
│  Login Server (UDP 5998)                                     │
│  World Server (UDP 9000)                                     │
│  Zone Server (UDP varies)                                    │
│    → OP_NEW_SPAWN, OP_CLIENT_UPDATE, OP_DELETE_SPAWN         │
│    → OP_HP_UPDATE, OP_NEW_ZONE, OP_ZONE_SPAWNS               │
│    → OP_CHANNEL_MESSAGE (GM commands via chat)               │
└────────────────────────┬─────────────────────────────────────┘
                         │ UDP (EQ protocol)
                         ▼
┌──────────────────────────────────────────────────────────────┐
│              eq_renderer (modified, standalone)               │
│                                                              │
│  ┌─────────────┐   ┌──────────────┐   ┌──────────────────┐ │
│  │ eq_net       │──▶│ GameState    │──▶│ SceneState       │ │
│  │ (new module) │   │ (new module) │   │ (existing)       │ │
│  │             │   │              │   │                  │ │
│  │ Login→World │   │ player pos   │   │ billboards       │ │
│  │ →Zone flow  │   │ entities map │   │ zone name        │ │
│  │ GM commands │   │ zone info    │   │ camera target    │ │
│  │ packet parse│   │ HP/level     │   │ messages         │ │
│  └─────────────┘   └──────────────┘   └──────────────────┘ │
│         │                                      │            │
│         │       ┌──────────────────────────────┘            │
│         ▼       ▼                                           │
│  ┌──────────────────────────────────────────────────────┐   │
│  │              Existing wgpu renderer                   │   │
│  │  (EqRenderer, camera, HUD, billboards, zone loading) │   │
│  └──────────────────────────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────┘
```

## What Changes vs. What Stays

### Removed
- `src/ipc.rs` — Unix socket listener, `BotMap`, `spawn_listener`, `spawn_multi_listener`
- `DEFAULT_SOCKET_PATH`, `socket_path_to_bot_id`, all IPC-related code
- Config.yaml `bot_sockets`, `camera_port`, `renderer.*` sections (replaced by `eq_net` config)
- The HTTP camera/frame server (`src/http.rs`) — was only needed for external bot control
- The bot selector HUD overlay

### Stays (unchanged)
- `src/scene.rs` — `SceneState`, `Billboard`, `LogEntry` (just populated from `GameState` instead of `GameStateMsg`)
- `src/renderer.rs` — `EqRenderer`, zone rendering, character models
- `src/camera.rs`, `src/camera_state.rs` — camera orbit/follow logic
- `src/hud.rs` — HUD drawing (minor: bot selector removed, simplified title)
- `src/assets.rs` — zone asset loading from S3D files
- `src/models.rs` — character model loading
- `src/billboard.rs` — entity billboard rendering
- All GPU/render pipeline code (`src/gpu.rs`, `src/pipeline.rs`, `src/pass.rs`, `src/anim.rs`, `src/debug_zone.rs`)

### New modules
- `src/eq_net/` — the entire EQ protocol client (see below)
- `src/game_state.rs` — in-game state mirroring the Python `GameState` class

## New Module: `src/eq_net/`

Ported from the Python reference implementation (`eq_client/connection/stream.py`
and `eq_client/protocol/`).

### `src/eq_net/mod.rs` — Public API

```rust
pub struct EqClient {
    // Internal state machine
}

impl EqClient {
    /// Create a new EQ client. Does not connect yet.
    pub fn new(config: EqConfig) -> Self;

    /// Run the full login→world→zone flow. Returns when zone is ready.
    pub async fn connect(&mut self) -> Result<(), EqError>;

    /// Send a GM command via in-game chat (e.g. "#goto Aethas", "#set hide_me on").
    pub fn send_gm_command(&self, cmd: &str);

    /// Send a raw chat message.
    pub fn send_chat(&self, msg: &str, channel: u32);

    /// Send a position update (OP_CLIENT_UPDATE) — used for GM #goto teleport completion.
    pub fn send_position_update(&self, x: f32, y: f32, z: f32, heading: f32);

    /// Non-blocking poll for new game state. Returns a snapshot of current state.
    /// Called once per frame from the render loop.
    pub fn poll_state(&mut self) -> Option<GameStateSnapshot>;

    /// Graceful disconnect.
    pub fn disconnect(&mut self);
}

pub struct EqConfig {
    pub login_host: String,
    pub login_port: u16,       // default 5998
    pub world_port: u16,       // default 9000
    pub username: String,
    pub password: String,
    pub character_name: String,
    pub target_player: String,  // player to follow via #goto
}
```

### `src/eq_net/transport.rs` — UDP stream + session management

Direct port of `eq_client/connection/stream.py` `EQStream` class:

- `EqStream::connect(host, port)` — UDP datagram endpoint + session request/response
- `EqStream::send_app_packet(opcode, payload)` — reliable sequenced + fragmented send
- `EqStream::poll_recv()` — non-blocking receive, returns parsed app packets
- CRC32 keyed encode/decode, XOR cipher, EQ compression (0x5a/0xa5 prefix)
- Fragment reassembly (`FragmentBuffer`)
- In-order delivery with ACKs, out-of-order buffering

Key difference from Python: uses `tokio::net::UdpSocket` instead of
`asyncio.DatagramProtocol`. Non-blocking poll model instead of callback-based.

### `src/eq_net/protocol.rs` — Opcodes + struct definitions

Port of `eq_client/protocol/opcodes.py` and `eq_client/protocol/structs.py`:

- All opcode constants (`OP_NEW_SPAWN`, `OP_CLIENT_UPDATE`, `OP_DELETE_SPAWN`, etc.)
- `Spawn_S` struct with bitfield position extraction (the 19-bit signed coord encoding)
- `SpawnPositionUpdate_S` — entity movement packets
- `HPUpdate_S`, `Death_S`, `NewZone_S`, `ZoneServerInfo_S`, `ClientZoneEntry_S`
- `SpawnAppearance_S` — animation/HP pct updates
- Race ID → renderer code mapping (`eq_race_to_code`)

All structs use `#[repr(C, packed)]` with `Copy` derive instead of ctypes.
Struct unpacking via `safe_read<T>(bytes: &[u8]) -> T` using `ptr::read_unaligned`.

### `src/eq_net/login.rs` — Login flow state machine

Port of `eq_client/game/login_flow.py`:

```
SessionReady → CredentialsSent → LoginAccepted → ServerListReceived
  → PlayEverquestSent → WorldConnected → CharInfoReceived
  → EnterWorldSent → ZoneServerInfo → ZoneConnected
  → ZoneEntrySent → (receive spawns) → ZoneReady
```

Handles:
- Login server: DES-encrypted credentials (using `des` crate)
- World server: `OP_SEND_LOGIN_INFO`, `OP_ENTER_WORLD`, character select
- Zone server: `OP_ZONE_ENTRY`, spawn processing, `OP_CLIENT_READY`
- GM commands on zone ready: `#set hide_me on`, `#goto <target_player>`
- Zone changes: detect `OP_REQUEST_CLIENT_ZONE_CHANGE` → reconnect to world
- Death/respawn: `OP_ZonePlayerToBind` → re-enter zone

### `src/eq_net/gm.rs` — GM-specific helper methods

```
send_hide_me(true)      → #set hide_me on
send_goto(name)         → #goto <name>
send_goto_entity(id)    → #goto <spawn_id>
send_gm_off()           → #gm off (if user wants to be visible)
```

These are convenience wrappers around `send_chat` that prepend `#`.

## New Module: `src/game_state.rs`

Port of `eq_client/game/state.py`. Owns all in-game state:

```rust
pub struct GameState {
    // Player
    pub player_id: u32,
    pub player_name: String,
    pub player_x: f32, pub player_y: f32, pub player_z: f32,
    pub player_heading: f32,
    pub player_level: u32,
    pub player_race: String,
    pub hp_pct: f32, pub mana_pct: f32, pub xp_pct: f32,

    // Zone
    pub zone_name: String,
    pub zone_id: u16,
    pub zone_changed: bool,

    // Entities (keyed by spawn_id)
    pub entities: HashMap<u32, Entity>,

    // Target
    pub target_id: Option<u32>,
    pub target_name: Option<String>,
    pub target_hp_pct: Option<f32>,

    // Messages
    pub messages: VecDeque<LogEntry>,  // ring buffer, max 50

    // Strategy text for HUD
    pub strategy: String,
}

pub struct Entity {
    pub spawn_id: u32,
    pub name: String,
    pub level: u32,
    pub is_npc: bool,
    pub x: f32, pub y: f32, pub z: f32,
    pub hp_pct: f32,
    pub race: String,
    pub heading: f32,
    pub dead: bool,
}
```

The `EqClient` updates this state from incoming packets. The render loop reads it
once per frame to build a `SceneState`.

## Modified: `src/main.rs`

```rust
#[tokio::main]
async fn main() {
    // 1. Read config.yaml (new format: eq_net section)
    let config = read_config();

    // 2. Connect to EQEmu
    let mut eq_client = EqClient::new(config.eq_net);
    eq_client.connect().await.expect("Failed to connect to EQEmu");

    // 3. Send GM commands to hide and follow target
    eq_client.send_gm_command("set hide_me on");
    eq_client.send_gm_command(&format!("goto {}", config.target_player));

    // 4. Run render loop (blocking, on main thread)
    //    Each frame: eq_client.poll_state() → SceneState::from_game_state() → render
    let event_loop = EventLoop::new().unwrap();
    let mut app = App::new(eq_client, assets_path, models_path);
    event_loop.run_app(&mut app).unwrap();
}
```

## Modified: `src/scene.rs`

Add a new conversion function:

```rust
impl SceneState {
    // Existing: from IPC message (kept for backward compat during development)
    pub fn from_msg(msg: &GameStateMsg) -> Self { ... }

    // New: from live game state
    pub fn from_game_state(gs: &GameState) -> Self {
        SceneState {
            zone: gs.zone_name.clone(),
            zone_changed: gs.zone_changed,
            player_pos: [gs.player_x, gs.player_y, gs.player_z],
            player_heading: gs.player_heading,
            player_hp_pct: gs.hp_pct,
            player_mana_pct: gs.mana_pct,
            player_xp_pct: gs.xp_pct,
            player_name: gs.player_name.clone(),
            player_level: gs.player_level,
            player_race: gs.player_race.clone(),
            target_name: gs.target_name.clone(),
            target_hp_pct: gs.target_hp_pct,
            strategy: gs.strategy.clone(),
            billboards: gs.entities.values().map(|e| Billboard::from(e)).collect(),
            messages: gs.messages.iter().cloned().collect(),
        }
    }
}
```

## Modified: `App` struct (main.rs)

```rust
struct App {
    // ... existing renderer/camera fields ...

    eq_client: EqClient,       // replaces bot_map + active_bot
    game_state: GameState,     // cached snapshot
}
```

Each frame:
1. `eq_client.poll_state()` — drains pending UDP packets, updates internal `GameState`
2. `self.game_state = eq_client.snapshot()` — get current state
3. `SceneState::from_game_state(&self.game_state)` — convert to renderer format
4. Existing render path unchanged

## Cargo.toml Changes

```toml
[dependencies]
# Remove (no longer needed):
# axum = ...
# serde_yaml = ...  (keep if we still use config.yaml)

# Add:
des = "0.8"           # DES encryption for login credentials
tokio = { version = "1", features = ["full"] }  # already present, ensure rt-multi-thread
rand = "0.8"          # for session connect_code
byteorder = "1.5"     # for network byte-order reads
```

## Config Format (config.yaml)

```yaml
# New top-level section replaces renderer.bot_sockets / renderer.camera_port
eq_net:
  login_host: "127.0.0.1"
  login_port: 5998
  world_port: 9000
  username: "gmaccount"
  password: "gmpassword"
  character_name: "GMObserver"
  target_player: "Aethas"    # player to follow

# Kept (renderer still needs these):
renderer:
  assets_path: "~/eq_assets"
  models_path: "~/eq_renderer/assets/models"
```

## Implementation Order

### Phase 1: Protocol Foundation
1. `src/eq_net/protocol.rs` — opcodes, struct definitions, `eq_race_to_code`
2. `src/eq_net/transport.rs` — UDP stream, session management, CRC/compression/XOR
3. Unit tests: CRC32, XOR encode/decode, compression round-trip, fragment reassembly

### Phase 2: Login Flow
4. `src/eq_net/login.rs` — login→world→zone state machine
5. `src/game_state.rs` — game state struct + entity management
6. Integration: connect to a real EQEmu server, verify zone entry completes

### Phase 3: GM Features + Entity Tracking
7. `src/eq_net/gm.rs` — GM command helpers
8. Spawn processing: `OP_NEW_SPAWN`, `OP_ZONE_SPAWNS`, `OP_DELETE_SPAWN`
9. Position updates: `OP_CLIENT_UPDATE` → entity movement
10. HP updates: `OP_HP_UPDATE`, `OP_DEATH`
11. Zone changes: `OP_REQUEST_CLIENT_ZONE_CHANGE` → reconnect flow

### Phase 4: Renderer Integration
12. `src/scene.rs` — add `from_game_state()`
13. `src/main.rs` — replace IPC setup with `EqClient`, wire up render loop
14. `src/hud.rs` — simplify (remove bot selector, show connection status)
15. Remove `src/ipc.rs`, `src/http.rs` (or keep behind feature flag)

### Phase 5: Polish
16. Config parsing for new `eq_net` section
17. Error handling: reconnection on disconnect, graceful shutdown
18. HUD: show "Connecting..." / "Following: <name>" / connection errors
19. Command-line override for target player (`--follow Aethas`)

## Verification

1. Start EQEmu server, log in a player character separately
2. `cargo run -- --follow Aethas`
3. Verify: renderer shows zone, GM character is invisible, camera follows target player
4. Verify: entities (NPCs, other players) appear and move in real-time
5. Verify: target player zones → GM follows to new zone automatically
6. Verify: HP updates, death events, chat messages display in HUD

## Decisions

1. **Login credentials**: Read from the per-character login config (`~/.config/eqoxide/`,
   see README) — the same `server` and `account` sections the Python reference uses.

2. **Reconnection strategy**: Auto-reconnect up to 3 times on any connection failure
   (login, world, or zone). After 3 failures, exit with an error message. Each retry
   starts the full login→world→zone flow from scratch.

3. **Multiple zone servers**: Handled by `OP_ZONE_SERVER_INFO` during login flow.
   No special handling needed.

4. **Titanium only**: Opcodes are for Titanium (port 5998). No other client versions.

5. **IPC removal**: `src/ipc.rs` and `src/http.rs` are deleted entirely. No feature flag,
   no backward compat. The renderer gets game state exclusively from the `EqClient`.
