# OP_TargetCommand (RoF2)

## Opcode
`OP_TargetCommand = 0x58e2` — confirmed in
`EQEmu/utils/patches/patch_RoF2.conf:233`. This matches the value the eqoxide
issue assumed.

Related target opcodes in the same patch file, for context (do not confuse
these with OP_TargetCommand):
- `OP_TargetHoTT = 0x0272` (`patch_RoF2.conf:243`) — "heroic/hunter's target of
  target" push, uses the same `ClientTarget_Struct` layout
  (`zone/client.cpp:4518-4519`).
- `OP_PetHoTT = 0x794a` (`patch_RoF2.conf:205`) — pet's target-of-target,
  also `ClientTarget_Struct` (`zone/npc.cpp:562-563`).
- `OP_TargetMouse = 0x075d` (`patch_RoF2.conf:240`) — client→server mouse-click
  target request, same struct, handled at `zone/client_packet.cpp:15127-15138`.
- `OP_Taunt = ...` also reuses `ClientTarget_Struct` for its size check
  (`zone/client_packet.cpp:15340-15341`).

## Wire struct: `ClientTarget_Struct`
Confirmed identical across every post-Titanium patch (titanium, sof, sod, uf,
rof, rof2) — a single `uint32`, no ENCODE/DECODE override needed for RoF2.

`EQEmu/common/patches/rof2_structs.h:1311-1318`:
```c
/*
** Client Target Struct
** Length: 2 Bytes      <-- stale/inaccurate comment, ignore; actual field is uint32
** OpCode: 6221
*/
struct ClientTarget_Struct {
/*000*/ uint32  new_target;         // Target ID
};
```
- **Size: 4 bytes.** (The struct's doc comment says "Length: 2 Bytes" — that's
  a leftover/incorrect comment; `sizeof(ClientTarget_Struct)` is used
  everywhere in `zone/client.cpp`/`zone/client_packet.cpp` for the actual
  4-byte `uint32` field, and that's what's queued on the wire.)
- Field: `new_target` — the target's entity/spawn ID (the same ID space as
  `Spawn_Struct.spawnId` / `Mob::GetID()`), a `uint32`, no other fields.
- No `common/patches/rof2.cpp` ENCODE/DECODE entry exists for
  `OP_TargetCommand` (grepped, not present) — it is sent/received as a raw
  struct with no server-side re-encoding for RoF2. Wire bytes = 2-byte opcode
  header (`e2 58` little-endian) + 4-byte `new_target` payload = 6 bytes
  total on the EQStream app layer.

## Direction: genuinely bidirectional
- **Client → server**: the client sends `OP_TargetCommand` itself when the
  player clicks/hotkeys a target. Confirmed in the RoF2 client decompile —
  `everquest_rof2/decompiled/ghidra/eqgame.exe.c:545036-545055` (dispatch
  `case 0x1b`) builds and sends opcode `0x58e2` with a 4-byte entity-id
  payload via `FUN_007d03c0(0x58e2,0)` / `FUN_008c51f0(4,&DAT_00dd00e0,6)`
  (6-byte total send = 2-byte opcode + 4-byte payload). Server receives it in
  `Client::Handle_OP_TargetCommand` (`zone/client_packet.cpp:15085-15113`),
  validates range/invisibility/bodytype, and on success **echoes the same
  packet back to the same client** via `QueuePacket(app)`
  (`zone/client_packet.cpp:15112`) as the target-accepted confirmation; on
  failure it sends `OP_TargetReject` instead (`zone/client_packet.cpp:15114-
  15119`).
- **Server → client (unprompted, forced target push)**: `Client::SendTarget-
  Command(uint32 EntityID)` (`zone/client.cpp:7004-7010`) builds and queues
  the packet directly, no client request involved. Confirmed call sites:
  - `LocateCorpse()` — corpse-locate finds the closest corpse and forces the
    client to target it (`zone/client.cpp:7012-7028`, calls
    `SendTargetCommand(ClosestCorpse->GetID())` at line 7027).
  - Sense Undead / Sense Summoned / Sense Animal spell effects — finds the
    closest mob of the relevant bodytype and forces target (SoD+ only, gated
    by `maskSoDAndLater`) (`zone/spell_effects.cpp:865-904`, the send is at
    line 896).
  - Mercenary-hire clears the player's target (see "id==0" below):
    `zone/client_packet.cpp:10670-10672`.
  - Exposed to quest Perl via `$client->SendTargetCommand($entity_id)`
    (`zone/perl_client.cpp:1580-1582,3823`) and to Lua as
    `TargetCommand` opcode constant (`zone/lua_packet.cpp:528`) — so any
    quest script can force a client's target this way.

## id == 0 sentinel = clear target
Confirmed: `zone/client_packet.cpp:10670-10672` — mercenary-hire handler does
`SetHoTT(0); SendTargetCommand(0);` specifically to clear the player's current
target/HoTT after opening the merc-hire window. `0` is not a valid spawn ID
(`entity_list.GetMob(0)` returns null both client- and server-side — there's
no special-cased branch for it, it just naturally resolves to "no target"
because 0 is outside the valid spawn-ID space). No other sentinel values
(e.g. 0xFFFFFFFF) appear anywhere in these call sites — only 0 is used as the
"clear" idiom.

## Recommendation for eqoxide
- Struct: single `u32` (LE) `new_target` field, 4-byte payload, opcode
  `0x58e2`. No RoF2-specific decode step — parse/serialize identically to any
  other patch.
- Implement OP_TargetCommand as a genuinely bidirectional handler:
  - **Inbound (server→client)**: when eqoxide receives `OP_TargetCommand` with
    a nonzero `new_target` that isn't the client's own current requested
    target, treat it as a forced-target push (corpse locate, sense spells,
    quest `SendTargetCommand`) — update local target state and target-of-
    target UI to match. When the client itself just sent an `OP_TargetCommand`
    request, the server's echo of the *same* packet back is the "target
    accepted" ack — no need for a separate ack opcode.
  - `new_target == 0` → clear the local target (set to `None`), matching the
    merc-hire-clear semantics.
  - If the server instead sends `OP_TargetReject` (opcode value at
    `patch_RoF2.conf:644` — currently listed as `0x0000`, worth
    double-checking against a fresh capture if you rely on it, since `0x0000`
    is often a "not yet reverse-engineered" placeholder in these conf files
    rather than a genuine wire value), treat that as "target request denied,
    revert to previous target" and surface the `DONT_SEE_TARGET` message
    string if you want message parity.
- Do not confuse this opcode's struct with `OP_TargetHoTT` (`0x0272`) or
  `OP_PetHoTT` (`0x794a`) — they reuse the identical `ClientTarget_Struct`
  layout but represent target-of-target and pet-target-of-target respectively,
  not the player's own primary target.
