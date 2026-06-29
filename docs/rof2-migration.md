# RoF2 Client Migration — Roadmap

**Goal:** Retarget eqoxide from EQ *Titanium* to *Rain of Fear 2 (RoF2)* — so the EQEmu
server identifies and drives us as a RoF2 client, and we parse/render RoF2's wire protocol
and assets.

## Why this is a big change

RoF2 differs from Titanium in: the **opcode table**, most **wire structs** (Spawn,
PlayerProfile, ClientUpdate/position, inventory + item serialization, zone entry), the
**inventory slot map** (RoF2 has many more slots / different ranges), and **asset formats**
(RoF2 uses the newer **EQG** container + model formats alongside S3D/WLD). We migrate in
dependency order; each phase leaves the client in a working, testable state.

## Ground-truth sources (this branch)

- **RoF2 client decompile** — `~/eq_assets/everquest_rof2/redacted/` (`REDACTED-TOOL/*.c`,
  `REDACTED-TOOL/*.asm`). Generated for the 2019 RoF2 build. Capstone done; Ghidra in progress.
- **EQEmu RoF2 patch** — `~/git/EQEmu/common/patches/`: `rof2_structs.h` (wire layout),
  `rof2.cpp` (encode/decode + opcode translation), `~/git/EQEmu/utils/patches/patch_RoF2.conf`
  (the opcode table). This is the definitive contract.
- **eq-client-expert agent** — repointed to RoF2; consult for any client-side behavior.

## How EQEmu decides we're a RoF2 client (the target of Phase 1)

EQEmu's stream identifier matches a registered **patch signature** against the first app
packet on each stream (`rof2.cpp:93–108`):

- **world stream** → first packet must be `OP_SendLoginInfo` (RoF2 `0x7a09`) of length
  `sizeof(LoginInfo_Struct)`. (RoF2 LoginInfo is 464 bytes — same size as Titanium; the
  **opcode value is what distinguishes RoF2** here.)
- **zone stream** → first packet must be `OP_ZoneEntry` (RoF2 `0x5089`) of length
  `sizeof(ClientZoneEntry_Struct)` = **76 bytes** (Titanium/our current = 68). The extra 8
  bytes (`unknown68`, `unknown72`) must be present or the signature won't match.

Send those two packets with RoF2 opcodes + sizes, and use the RoF2 opcode table for
everything else, and the server treats the whole session as RoF2.

---

## Phases (dependency order)

### Phase 1 — Opcodes + handshake recognition  ← START HERE
Make the server identify us as RoF2 and exchange the login→world→zone handshake.

- Replace every opcode constant in `src/eq_net/protocol.rs` (currently ~111 Titanium values)
  with the RoF2 values from `patch_RoF2.conf`. Keep the same Rust constant *names* (the app
  layer is unchanged); only the numeric values change. Drive this from the conf file, not by
  hand, to avoid transcription errors.
- Fix the handshake struct sizes so the signatures match: `SIZE_CLIENT_ZONE_ENTRY` 68 → 76
  (add the two trailing u32s); confirm `SIZE_LOGIN_INFO` (464) matches RoF2 and the
  `login_info` field packing is identical.
- Verify the login/world/zone state machine (`src/eq_net/login.rs`) still completes against a
  RoF2-configured EQEmu — the server logs `[StreamIdentify] Registered patch [RoF2]` and
  accepts our world + zone streams as RoF2.
- **Done when:** the client logs in and zones in against EQEmu and the world/zone logs show it
  identified as RoF2 (not Titanium, not "unidentified"). Expect *downstream* parsing to be
  wrong until later phases — that's fine; the gate is RoF2 identification + handshake.

### Phase 2 — Core gameplay structs: zone entry, spawns, position
- `Spawn_Struct` (RoF2 layout — different field order/sizes, bitfields), `PlayerProfile_Struct`,
  `OP_ClientUpdate` position (RoF2 bit-packed update differs), `NewZone_Struct`. Update the
  decoders in `src/eq_net/` + `src/game_state.rs` against `rof2_structs.h`.
- **Done when:** spawns appear at correct positions, the player zones in at the right spot,
  NPC movement is smooth.

### Phase 3 — Inventory + items
- RoF2 inventory slot map (larger; different ranges) and the RoF2 **item serialization**
  (different from Titanium's). Update `OP_CharInventory`, `OP_ItemPacket`, the slot math, and
  any `/v1/observe/inventory`/`/v1/interact/give`/`/trade` wire code.
- **Done when:** inventory reads correctly and equip/give/merchant flows work.

### Phase 4 — Appearance + wearchange + spell/combat opcodes
- `WearChange_Struct`, `SpawnAppearance`, animation/combat opcodes, casting. Mostly opcode +
  struct deltas on top of Phase 2/3.

### Phase 5 — Assets (EQG)
- RoF2 ships many models/zones as **EQG** (newer container + `.mod`/`.ter`/`.zon` formats),
  not S3D/WLD. Extend the asset pipeline (`eqoxide_asset_server`) to read EQG. Large; can lag
  the protocol work since the client renders from converted GLBs regardless of source format.

### Phase 6 — Polish / parity
- Tasks, merchant, quests, doors, etc. — re-verify each against RoF2 structs; fix deltas.

---

## Phase 1 detailed plan (opcodes + handshake)

**Files:** `src/eq_net/protocol.rs` (opcode consts + handshake struct sizes),
`src/eq_net/login.rs` (handshake send), possibly `src/eq_net/transport.rs` (if any opcode is
referenced there). Reference: `~/git/EQEmu/utils/patches/patch_RoF2.conf`,
`~/git/EQEmu/common/patches/rof2_structs.h`.

1. **Extract the RoF2 opcode table.** Parse `patch_RoF2.conf` (`OP_Name=0xValue` lines) into a
   name→value map. For each `pub const OP_X: u16 = 0x....;` in `protocol.rs`, look up the RoF2
   value and replace it. Flag any eqoxide opcode constant that has **no** entry in the RoF2
   conf (it may be named differently in RoF2, or be a transport-layer op that doesn't live in
   the conf) and resolve each by hand against the conf / `rof2.cpp`. Keep names stable.
   - Note the app-layer enum name mapping: eqoxide uses names like `OP_SEND_LOGIN_INFO`; the
     conf uses `OP_SendLoginInfo`. Map case-insensitively / by the canonical EQEmu name.
2. **Handshake struct sizes:** set `SIZE_CLIENT_ZONE_ENTRY = 76` and update the
   `ClientZoneEntry` builder in `login.rs` to write the 76-byte layout (u32 + char[64] + u32 +
   u32; char name at offset 4). Verify `SIZE_LOGIN_INFO`/the LoginInfo builder against RoF2.
3. **Transport opcodes** (session layer: OP_SessionRequest/Response, OP_Combined, OP_Packet,
   OP_Fragment, OP_Ack, etc.) are **protocol-layer** and identical across patches — do NOT
   change those; only the *application* opcodes from the conf change.
4. **Verify** against a RoF2-configured local EQEmu: confirm `[StreamIdentify] ... [RoF2]` in
   the world/zone logs and that the client completes the zone-in handshake. (EQEmu must be
   running its RoF2 patch — it registers all patches and matches by signature, so no server
   config change should be needed beyond having the RoF2 patch present, which it does.)

**Tests:** a unit test that the RoF2 opcode table is internally consistent (no duplicate
values, all expected handshake opcodes present with the conf's values); the real gate is the
live RoF2-identification check above.

**Risk:** some opcodes eqoxide uses may not exist or be renamed in RoF2; the conf is
authoritative — anything unmapped is investigated via `rof2.cpp` / the decompile / the expert.
