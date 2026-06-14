# eq_client_lite: Rust GM Observer Client for EQEmu

## Context

`eq_client_lite` is a copy of `eq_renderer` modified to connect directly to the EQEmu server as a GM-level game client instead of receiving game state from a Python bot via Unix socket. It logs in as a GM character, uses GM commands to follow a target player (teleport + invisibility), receives real-time entity position updates via the EQ protocol, and renders the 3D scene — **zero server modifications required**.

The `aiquestbot` character will be promoted to GM status in the database.

## Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     EQEmu Server (unchanged)                 │
│                                                              │
│  Login Server (5998) ──► World Server (9000) ──► Zone Server│
│  (auth)                   (char select,                      │
│                           /goto, zoning)                     │
└──────────────┬───────────────────────────────────────────────┘
               │  EQ Protocol (UDP + XOR stream)
               │  - Login with GM account
               │  - Select character "aiquestbot"
               │  - /goto <target_player>  (teleport to player)
               │  - #hide_me               (invisible to others)
               │  - Receive OP_SpawnPositionUpdate for entities
               │  - Receive OP_NewSpawn / OP_DeleteSpawn
               │
               ▼
┌──────────────────────────────────────────────────────────────┐
│                    eq_client_lite (Rust)                     │
│                                                              │
│  ┌─────────────────┐    ┌─────────────────────────────────┐ │
│  │  eq_protocol     │    │  renderer (from eq_renderer)    │ │
│  │  - LoginClient   │───►│  - reads GameStateMsg from      │ │
│  │  - WorldClient   │    │    BotMap (same as before)      │ │
│  │  - ZoneClient    │    │  - renders zone + entities      │ │
│  │  - XOR codec     │    │  - AutoFollows player           │ │
│  │  - CRC32         │    │  - loads S3D zone geometry      │ │
│  └─────────────────┘    └─────────────────────────────────┘ │
│                                                              │
│  Flow:                                                       │
│  1. Connect → login → world → zone (select aiq[...]         │
│  2. /goto <target> → teleport to player                      │
│  3. Loop:                                                    │
│     a. Receive spawn position updates from server            │
│     b. Build GameStateMsg from EQ protocol data              │
│     c. Insert into BotMap (same format as RendererClient)    │
│     d. Renderer reads BotMap → renders frame                 │
│     e. If player zones → detect via position change →        │
│        /goto again                                           │
│  4. Camera follows player automatically                      │
└──────────────────────────────────────────────────────────────┘
```

## What Gets Reused From eq_renderer (Unchanged)

| Module | Purpose |
|--------|---------|
| `renderer.rs` | GPU rendering coordinator |
| `pass.rs` | Render passes (sky, zone, billboard, entities) |
| `pipeline.rs` | wgpu pipeline construction |
| `gpu.rs` | GPU types and upload |
| `assets.rs` | S3D/WLD zone geometry loading |
| `models.rs` | glTF model loading |
| `anim.rs` | Skeletal animation |
| `camera.rs` | Camera math |
| `camera_state.rs` | AutoFollow camera |
| `scene.rs` | `GameStateMsg` → `SceneState` conversion |
| `hud.rs` | egui HUD overlay |
| `http.rs` | Axum HTTP camera control |
| `billboard.rs` | Billboard geometry |
| `debug_zone.rs` | Test zone |
| `shaders/` | All WGSL shaders |

## What Gets Replaced/Modified

| Original | Replacement |
|----------|-------------|
| `ipc.rs` (Unix socket listener) | `eq_protocol/` module (EQ protocol client) |
| `main.rs` (spawns IPC thread) | `main.rs` (spawns EQ protocol thread) |

## New Module: `eq_protocol/`

A new directory `src/eq_protocol/` implementing the EQ client protocol in Rust:

### `eq_protocol/mod.rs` — Connection orchestrator
- `EqClient` struct that manages the full connection lifecycle:
  1. Connect to login server (UDP 5998) → authenticate
  2. Get world server address → connect to world (UDP)
  3. Send `OP_SendLoginInfo` → get character list
  4. Send `OP_EnterWorld` with character name
  5. Receive `OP_ZoneServerInfo` → connect to zone server (UDP)
  6. Send `OP_ZoneEntry` → receive spawns + position updates
  7. Enter main loop: parse packets, build `GameStateMsg`, insert into `BotMap`

### `eq_protocol/login.rs` — Login server protocol
- UDP connection to port 5998
- `OP_SessionRequest` / `OP_SessionResponse` (XOR key exchange)
- `OP_Login` with DES-CBC encrypted credentials (zero key)
- `OP_ServerListRequest` / `OP_PlayEverquestRequest` to get world server address

### `eq_protocol/world.rs` — World server protocol
- UDP connection to world server port
- `OP_SendLoginInfo` (send account ID + session key)
- `OP_EnterWorld` (select character)
- `OP_ZoneServerInfo` reception (get zone server address)
- GM command: send `OP_GMGoto` to teleport to target player
- GM command: send `OP_GMHideMe` to become invisible

### `eq_protocol/zone.rs` — Zone server protocol
- UDP connection to zone server
- `OP_ZoneEntry` (enter zone)
- Parse `OP_ZoneSpawns` (bulk entity spawn data)
- Parse `OP_NewSpawn` / `OP_DeleteSpawn` (entity appear/disappear)
- Parse `OP_SpawnPositionUpdate` (entity position updates — the key data source)
- Parse `OP_SpawnAppearance` (entity state changes)
- Parse `OP_NewZone` (zone name for geometry loading)
- Send `OP_ClientUpdate` (our own position updates as we teleport around)

### `eq_protocol/codec.rs` — Protocol codec
- Reliable UDP stream (sequence numbers, ACKs, retransmit)
- XOR encoding/decoding (running key cipher)
- CRC32 checksum
- Zlib compression/decompression
- Packet framing (combine, fragment, parse)

### `eq_protocol/structs.rs` — Packet structures
- All EQ protocol structs as Rust types with `#[derive(Pod, Zeroable)]` for bytemuck
- `Spawn_Struct`, `PlayerPositionUpdateClient_Struct`, `PlayerPositionUpdateServer_Struct`, `LoginInfo`, etc.

### `eq_protocol/opcodes.rs` — Opcode constants
- All EQ protocol opcode values

## Modified Files

### `src/main.rs`
**Before:** Spawns tokio thread running `ipc::spawn_multi_listener()` to read Unix socket.
**After:** Spawns a thread running `EqClient::run()` that connects to the EQEmu server via EQ protocol and populates the `BotMap`.

Key changes:
1. Remove `use crate::ipc;` 
2. Add `use crate::eq_protocol::EqClient;`
3. Parse new CLI args: `--login-host`, `--login-port`, `--account`, `--password`, `--character`, `--target-player`
4. Instead of spawning IPC listener, spawn:
   ```std
   let bot_map_clone = bot_map.clone();
   std::thread::spawn(move || {
       let mut client = EqClient::new(login_host, login_port, account, password, character, target_player);
       client.run(bot_map_clone);
   });
   ```
5. Everything else (winit loop, rendering, camera, HUD) stays identical

### `Cargo.toml`
Add dependencies:
- `des = "0.8"` — DES-CBC encryption for login
- `rand = "0.8"` — random connect codes
- `byteorder = "1.5"` — byte-order conversion for protocol structs

## Connection & Following Strategy

### Phase 1: Connect and Enter World
1. Connect to login server, authenticate, get world server address
2. Connect to world server, send login info, get character list
3. Enter world with the GM character
4. Receive zone server address, connect to zone

### Phase 2: Follow Target Player
1. Send `OP_GMGoto` with target player's name → teleport to them
2. Send `OP_GMHideMe` → become invisible
3. Receive `OP_SpawnPositionUpdate` packets for all nearby entities
4. Build `GameStateMsg` from spawn data + position updates

### Phase 3: Cross-Zone Following
When the target player zones:
1. The player's spawn disappears (we stop receiving position updates for them)
2. We detect this (no update for target's spawn_id within timeout)
3. Send `OP_GMGoto` again → world server handles cross-zone teleport
4. Server sends `OP_ZoneServerInfo` for new zone → we reconnect to new zone server
5. Renderer detects `zone_changed` and loads new zone geometry
6. Resume receiving position updates in new zone

### Position Data Flow (Per Frame)
```
Zone Server → OP_SpawnPositionUpdate → eq_protocol/zone.rs
  → Extract spawn_id, x, y, z, heading
  → Update internal entity map
  → Every 100ms: build GameStateMsg {
      zone: current_zone_name,
      zone_changed: detected_zone_change,
      player: PlayerMsg { pos: [x, y, z], heading, name, level, hp_pct, race, action },
      entities: Vec<EntityMsg> { id, name, pos, heading, race, is_npc, level, hp_pct, ... },
      target: current_target,
      messages: recent_chat/combat,
    }
  → bot_map.lock().unwrap().insert("default", msg)
  → Renderer reads bot_map → renders frame
```

## Implementation Steps

### Step 1: Project Setup
- [x] Copy `eq_renderer` to `~/git/eq_client_lite/`
- [ ] Update `Cargo.toml` with new dependencies (`des`, `rand`, `byteorder`)
- [ ] Create `src/eq_protocol/` directory with module files
- [ ] Update `main.rs` to declare `eq_protocol` module

### Step 2: Protocol Codec (`codec.rs`)
- [ ] Implement reliable UDP connection (UDP socket + sequence tracking)
- [ ] Implement XOR encoding/decoding
- [ ] Implement CRC32 checksum
- [ ] Implement zlib compression/decompression
- [ ] Implement packet combine/fragment reassembly
- [ ] Unit test: encode/decode round-trip

### Step 3: Login Client (`login.rs`)
- [ ] Implement `OP_SessionRequest` / `OP_SessionResponse` handshake
- [ ] Implement DES-CBC encryption (zero key) for credentials
- [ ] Implement `OP_Login` → `OP_LoginAccepted` → extract session key
- [ ] Implement `OP_ServerListRequest` → `OP_PlayEverquestRequest` → get world address
- [ ] Integration test: connect to login server (if available)

### Step 4: World Client (`world.rs`)
- [ ] Implement `OP_SendLoginInfo` (send account ID + session key)
- [ ] Parse `OP_SendCharInfo` (character list)
- [ ] Implement `OP_EnterWorld` (select character)
- [ ] Parse `OP_ZoneServerInfo` (get zone server address)
- [ ] Implement `OP_GMGoto` (teleport to player)
- [ ] Implement `OP_GMHideMe` (invisibility)

### Step 5: Zone Client (`zone.rs`)
- [ ] Implement `OP_ZoneEntry` (enter zone)
- [ ] Parse `OP_ZoneSpawns` (bulk spawn data — initial entity state)
- [ ] Parse `OP_NewSpawn` / `OP_DeleteSpawn` (entity lifecycle)
- [ ] Parse `OP_SpawnPositionUpdate` (real-time position updates)
- [ ] Parse `OP_NewZone` (zone name)
- [ ] Build `GameStateMsg` from accumulated entity state
- [ ] Insert into `BotMap`

### Step 6: Main Loop Integration (`main.rs`)
- [ ] Replace IPC thread with EQ protocol thread
- [ ] Add CLI args for connection parameters
- [ ] Wire up `EqClient` to populate `BotMap`
- [ ] Test: renderer shows zone + entities from live server

### Step 7: Cross-Zone Following
- [ ] Detect target player zone-out (spawn disappears)
- [ ] Re-send `OP_GMGoto` to follow to new zone
- [ ] Handle zone server reconnect
- [ ] Set `zone_changed: true` for one frame to trigger geometry reload

### Step 8: Polish
- [ ] Handle disconnections and reconnection
- [ ] Add logging for protocol events
- [ ] Add `/target` command support
- [ ] Tweak camera for observer perspective

## Files to Create

| File | Purpose |
|------|---------|
| `src/eq_protocol/mod.rs` | `EqClient` — connection lifecycle, login→world→zone orchestration |
| `src/eq_protocol/codec.rs` | Reliable UDP stream, XOR codec, CRC32, zlib compression |
| `src/eq_protocol/login.rs` | Login server protocol (auth, DES encryption, server select) |
| `src/eq_protocol/world.rs` | World server protocol (char select, enter world, GM commands) |
| `src/eq_protocol/zone.rs` | Zone server protocol (spawn tracking, position updates, GameStateMsg builder) |
| `src/eq_protocol/structs.rs` | EQ protocol packet structures as Rust types |
| `src/eq_protocol/opcodes.rs` | All opcode constant definitions |

## Files to Modify

| File | Change |
|------|--------|
| `src/main.rs` | Replace IPC thread with EQ protocol thread, add CLI args |
| `Cargo.toml` | Add `des`, `rand`, `byteorder` dependencies |

## Files to Delete

| File | Reason |
|------|--------|
| `src/ipc.rs` | Replaced by `eq_protocol/` — no more Unix socket |

## Verification

1. **Build:** `cargo build` compiles without errors
2. **Connect:** Run with valid credentials, verify login → world → zone connection
3. **Observe:** Renderer shows the zone with player and NPC entities visible
4. **Follow:** Target player moves, verify camera follows smoothly
5. **Zone:** Target player zones, verify renderer follows to new zone
6. **Invisible:** Verify other players cannot see the GM observer
