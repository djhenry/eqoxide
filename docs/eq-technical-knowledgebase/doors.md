# Door System — Titanium Wire Protocol & Server Logic

Investigated 2026-06-23. All findings confirmed from EQEmu source and Titanium opcodes.conf.

---

## 1. How Doors Are Loaded

Doors live in the **database** (table `doors`), NOT embedded in zone .s3d geometry.
The server reads them at zone boot via `Zone::LoadZoneDoors()` (zone/zone.cpp:937–950):

```cpp
auto door_entries = content_db.LoadDoors(GetShortName(), GetInstanceVersion());
for (const auto &entry : door_entries) {
    auto d = new Doors(entry);
    entity_list.AddDoor(d);
}
```

`LoadDoors` queries:
```sql
SELECT * FROM doors WHERE zone = '<name>' AND (version = N OR version = -1) ORDER BY doorid ASC
```
(`common/repositories/base/base_doors_repository.h:309`)

### DB fields that become wire fields

| DB column       | Wire field        | Type     | Notes                              |
|-----------------|-------------------|----------|------------------------------------|
| `name`          | `name[32]`        | char[32] | Model filename, e.g. "DOOR1"       |
| `pos_y/x/z`     | `yPos/xPos/zPos`  | float    | World position                     |
| `heading`       | `heading`         | float    | Direction (EQ heading units)       |
| `incline`       | `incline`         | uint32   | Rotates the whole door model       |
| `size`          | `size`            | uint16   | 100 = normal scale                 |
| `doorid`        | `doorId`          | uint8    | Wire door ID (per-zone, 0–255)     |
| `opentype`      | `opentype`        | uint8    | Animation class (see below)        |
| `doorisopen`    | `state_at_spawn`  | uint8    | Current open/closed state          |
| `invert_state`  | `invert_state`    | uint8    | If 1, door is normally open        |
| `door_param`    | `door_param`      | uint32   | Extra data (merchant ID in bazaar) |
| (various)       | `unknown0052[12]` | bytes    | bytes [9] and [11] set to 0x01    |

Additional DB-only fields (NOT sent over wire, server-side logic only):
- `keyitem` (uint32) — item ID required to open
- `lockpick` (int16) — skill level needed to pick the lock
- `nokeyring` (uint8) — 1 = don't add key to keyring after use
- `guild` (int16) — guild ID restriction
- `triggerdoor` (int16) — chain-triggers this other door_id
- `triggertype` (int16) — 0=click triggers other, 1=other triggers this, 255=not clickable
- `dest_zone` (string) — destination zone short name for teleport doors
- `dest_x/y/z/heading` — destination coordinates
- `dest_instance` — destination instance ID
- `close_timer_ms` (int16) — auto-close delay in ms; default 5000
- `disable_timer` (int8) — 1 = never auto-close
- `is_ldon_door` — LDoN adventure port flag
- `client_version_mask` — bitmask filtering which client patches see this door

---

## 2. Wire Protocol

### Titanium Opcode Values (from utils/patches/opcodes.conf)
- `OP_SpawnDoor = 0x4c24`   — server→client, sent on ReqClientSpawn (zone-in)
- `OP_MoveDoor  = 0x700d`   — server→client, door open/close command
- `OP_ClickDoor = 0x043b`   — client→server, player clicked a door

### OP_SpawnDoor (0x4c24) — server→client

Sent in response to `OP_ReqClientSpawn` (the "ready for world data" request), inside
`Client::Handle_Connect_OP_ReqClientSpawn` (zone/client_packet.cpp:1077–1098).
The packet is a flat array of `Door_Struct` entries, one per door in the zone.

`Door_Struct` layout (80 bytes, titanium_structs.h:2158–2183):
```
offset  size  field
0x00    32    name[32]       — model filename string, null-padded
0x20     4    yPos (float)
0x24     4    xPos (float)
0x28     4    zPos (float)
0x2c     4    heading (float)
0x30     4    incline (uint32)
0x34     2    size (uint16)   — 100 = normal
0x36     6    unknown[6]      — zeros
0x3c     1    doorId (uint8)  — per-zone door number
0x3d     1    opentype (uint8)
0x3e     1    state_at_spawn  — computed: invert_state ? !is_open : is_open
0x3f     1    invert_state
0x40     4    door_param (uint32)
0x44    12    unknown0052[12] — bytes [9] and [11] set to 0x01; rest zeros
```

Total packet = N * 80 bytes (max 500 doors, entity.cpp:948).
`unknown0052[9]` and `unknown0052[11]` are always set to 1 (entity.cpp:991-992).

### OP_ClickDoor (0x043b) — client→server

16-byte struct (`ClickDoor_Struct`, titanium_structs.h:2195–2206):
```
offset  size  field
0x00     1    doorid          — which door was clicked
0x01     1    unknown001      — possibly action type
0x02     1    unknown002      — set after lever is closed
0x03     1    unknown003      — usually 0
0x04     1    picklockskill   — player's current pick lock skill
0x05     3    unknown005[3]
0x08     4    item_id         — item on cursor (for key checks)
0x0c     2    player_id       — spawner/entity ID of the player
0x0e     2    unknown014[2]
```

The server validates size == 16 exactly (client_packet.cpp:4658).
The server uses only `doorid` and `picklockskill` from this struct for game logic.
The `item_id` field is present but the server actually reads the player's inventory
directly via `sender->GetInv()`, not from this field.

### OP_MoveDoor (0x700d) — server→client

2-byte struct (`MoveDoor_Struct`, titanium_structs.h:2208–2211):
```
offset  size  field
0x00     1    doorid    — which door to animate
0x01     1    action    — 0x02 = open, 0x03 = close (or reversed for invert_state)
```

Action byte constants (doors.cpp:37–40):
```
OPEN_DOOR    = 0x02
CLOSE_DOOR   = 0x03
OPEN_INVDOOR = 0x03   // for invert_state==1 doors (normally-open)
CLOSE_INVDOOR= 0x02
```

For a normal door (invert_state=0): action 0x02 = open, 0x03 = close.
For an inverted door (invert_state=1): action 0x03 = open, 0x02 = close.

Broadcast: `entity_list.QueueClients()` — all zone clients see the same MoveDoor.

---

## 3. opentype Semantics

The opentype byte is purely a **client-side animation class hint**. The server does NOT
have a complete table of what each value means visually — that's baked into the client
binary. What the server does know:

| opentype | Server behavior notes (doors.cpp)                                      |
|----------|------------------------------------------------------------------------|
| 5        | DOOR1 — standard hinged door (swing)                                   |
| 40       | Special: auto-close via Process() timer even when trigger_type==1;     |
|          | NPCs will NOT open this (Open() guard); used for Corathus evolve XP   |
| 55       | BBBOARD — bulletin board (no swing, interact only)                     |
| 56       | CHEST1 — chest animation                                               |
| 57       | Teleport disc/portal — triggers MovePC on HandleClick (lines 541–611)  |
| 58       | Teleport + re-clickable when open (m_open_type==58 special cases);     |
|          | NPCs and ToggleState() skip this; always re-openable                   |
| 59       | NPCs cannot open this (Open() guard line 623)                          |
| 66       | PORT1414 — portal model in Qeynos                                      |
| 100      | QEYLAMP — lamp/light object                                            |

The key server-side opentype distinction:
- **57 or 58 + HasDestinationZone()**: door acts as a teleport/zone portal. Server calls
  `sender->MovePC(dest_zone, dest_x, dest_y, dest_z, dest_heading)` after sending MoveDoor.
- **40**: auto-close fires from timer in Process(), sends MoveDoor CLOSE to all clients.
- All other values: server just sends MoveDoor open/close; client handles the animation.

The client itself interprets the opentype to decide HOW to animate (swing, slide, etc.).
The original Titanium game client (`eqgame.exe`) does not expose readable door symbols
(the binary is stripped and the string "door" doesn't appear). The animation logic lives
in the stripped client binary and is not recoverable without a symbol server. However, based on
the model names in the struct comments and EQ community knowledge:
- Low values (1–10): hinged swing doors
- Values 40+: vary — slides, chests, boards, portals
- The client likely indexes into an animation table by opentype.

---

## 4. Auto-Close Logic (Server-Driven)

The server drives close, not the client. After opening, the server starts a timer:
- Timer duration = `door.close_timer_ms` from DB; default = 5000 ms
  (`base_doors_repository.h:237`)
- If `disable_timer` flag is set, the timer is never started (stays open until clicked again)
- When timer fires (Process(), doors.cpp:140–155):
  - If `open_type == 40` OR `triggertype == 1`: server sends `OP_MoveDoor` CLOSE to all clients
  - Sets open state back to false
- opentype==58 doors are re-openable while open (is_door_open_and_open_able special case)

---

## 5. Teleport/Zone-Portal Doors

Condition: `EQ::ValueWithin(m_open_type, 57, 58) && HasDestinationZone()` (doors.cpp:542).

When triggered:
1. Server sends OP_MoveDoor open to all clients (visual)
2. Server calls `sender->MovePC(dest_zone_id, dest_instance, dest_x, dest_y, dest_z, dest_heading)`
3. MovePC sends `OP_RequestClientZoneChange` to client (zoning.cpp:974)
4. Client processes zone change normally

For same-zone teleports (dest_zone == current zone): uses current instance ID to avoid
forcing a full zone change (doors.cpp:81–85).

---

## 6. Lock/Key/Picklock Logic (server-side, no wire impact for client)

All evaluated in HandleClick() (doors.cpp:158–617):
1. `lockpick==0 && keyitem==0 && guild==0` → unlocked; send MoveDoor open immediately
2. `keyitem != 0` → check player inventory (or key ring) for matching item; if found, open
3. `lockpick != 0` → check if player has ItemTypeLockPick on cursor and sufficient skill
4. `guild != 0` → check if player is in specified guild
5. `triggertype == 255` → not player-clickable; only opened by trigger from another door

The ClickDoor_Struct's `picklockskill` field is available but the server reads the skill
directly from `sender->GetSkill(EQ::skills::SkillPickLock)` — the field may be used for
something else or is vestigial.

---

## 7. Distance Gate

Server checks distance from player to door: `DistanceNoZ < RuleI(Range, MaxDistanceToClickDoors)`.
Distant clicks still pass to HandleClick() but quest scripts are suppressed (client_packet.cpp:4700).
This means "client-controlled" doors (some doors the client autonomously re-clicks to sync state)
work across distance — the server just skips the quest event, not the door action.

---

## 8. eq_client_lite Current State

- `OP_SPAWN_DOOR = 0x4c24` defined in `src/eq_net/protocol.rs:78`
- 0x4c24 is silenced (not parsed) in `src/eq_net/login.rs:364` — logged as "handled visually elsewhere" but not actually handled yet
- `OP_MoveDoor = 0x700d` — NOT yet defined or handled; currently silenced as "keepalive/time-sync" comment (login.rs:362, WRONG LABEL — 0x700d is MoveDoor, not keepalive)
- `OP_ClickDoor = 0x043b` — NOT defined or sent

**Bug in login.rs:362**: comment says 0x700d is "server keepalive/time-sync" but it is actually `OP_MoveDoor`. Need to fix the label and parse it.

---

## Wire Protocol Summary

| Opcode | Value  | Direction      | Purpose                                    |
|--------|--------|----------------|--------------------------------------------|
| OP_SpawnDoor | 0x4c24 | server→client | Sent on zone-in; array of all zone doors  |
| OP_ClickDoor | 0x043b | client→server | Player clicked a door (doorid + picklock) |
| OP_MoveDoor  | 0x700d | server→client | Door open/close animation command         |

- **OP_SpawnDoor (0x4c24)**: Flat array of 80-byte `Door_Struct` entries; sent once per zone-in in `OP_ReqClientSpawn` response.
- **OP_ClickDoor (0x043b)**: 16-byte struct; client sends doorid + player's picklock skill; server validates and replies with OP_MoveDoor.
- **OP_MoveDoor (0x700d)**: 2-byte struct (doorid + action); server broadcasts to all clients. Action 0x02=open, 0x03=close (reversed for invert_state doors).

---

## Sources

All paths below are in the [EQEmu/Server](https://github.com/EQEmu/Server) repository:

- [`common/patches/titanium_structs.h`](https://github.com/EQEmu/Server/blob/master/common/patches/titanium_structs.h) (lines 2151–2211) — Door_Struct, ClickDoor_Struct, MoveDoor_Struct, DoorSpawns_Struct
- [`utils/patches/opcodes.conf`](https://github.com/EQEmu/Server/blob/master/utils/patches/opcodes.conf) — Titanium opcode values
- [`zone/doors.cpp`](https://github.com/EQEmu/Server/blob/master/zone/doors.cpp) (lines 37–617) — HandleClick, Process, Open, ForceOpen, ForceClose, ToggleState
- [`zone/doors.cpp`](https://github.com/EQEmu/Server/blob/master/zone/doors.cpp) (lines 775–785) — LoadDoors DB query
- [`zone/entity.cpp`](https://github.com/EQEmu/Server/blob/master/zone/entity.cpp) (lines 932–1001) — MakeDoorSpawnPacket (OP_SpawnDoor assembly)
- [`zone/entity.cpp`](https://github.com/EQEmu/Server/blob/master/zone/entity.cpp) (lines 2703–2724) — DespawnAllDoors / RespawnAllDoors
- [`zone/client_packet.cpp`](https://github.com/EQEmu/Server/blob/master/zone/client_packet.cpp) (lines 1077–1098) — where SpawnDoor is sent (ReqClientSpawn handler)
- [`zone/client_packet.cpp`](https://github.com/EQEmu/Server/blob/master/zone/client_packet.cpp) (lines 4656–4720) — Handle_OP_ClickDoor
- [`zone/zone.cpp`](https://github.com/EQEmu/Server/blob/master/zone/zone.cpp) (lines 937–950) — Zone::LoadZoneDoors
- [`zone/zoning.cpp`](https://github.com/EQEmu/Server/blob/master/zone/zoning.cpp) (lines 973–997) — MovePC → OP_RequestClientZoneChange
- [`common/repositories/base/base_doors_repository.h`](https://github.com/EQEmu/Server/blob/master/common/repositories/base/base_doors_repository.h) (lines 38–76) — full DB schema
