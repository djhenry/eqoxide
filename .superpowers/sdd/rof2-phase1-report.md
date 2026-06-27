# RoF2 Phase 1 Implementation Report

## Status: DONE

## What Changed

### `src/eq_net/protocol.rs`
- Updated module doc comment to reflect RoF2 (was Titanium).
- Remapped **102 application opcodes** (`pub const OP_*: u16`) from Titanium values to RoF2 values sourced from `~/git/EQEmu/utils/patches/patch_RoF2.conf`.
- Left **10 NO-MATCH opcodes** with their current values and `// RoF2: NO MATCH IN CONF — needs manual resolution` comments (see below).
- `SIZE_CLIENT_ZONE_ENTRY`: 68 → **76** (RoF2 adds `unknown68: u32` + `unknown72: u32`).
- `ClientZoneEntry_S` struct: added `unknown68: u32` and `unknown72: u32` fields.
- Added `const _: () = assert!(std::mem::size_of::<ClientZoneEntry_S>() == 76)` compile-time guard.
- `SIZE_LOGIN_INFO`: unchanged at **464** — confirmed matches RoF2 `LoginInfo_Struct` (64+124+1+275 = 464).
- `LoginInfo_S` struct: unchanged — layout matches RoF2 exactly (`login_info[64]` at offset 0, `zoning` at offset 188).
- Added 4 new unit tests in `mod tests`: `rof2_handshake_opcodes_match_conf`, `rof2_client_zone_entry_size`, `rof2_login_info_size`, `rof2_zone_entry_builder_writes_name_at_offset_4`.

### `src/eq_net/login.rs`
- Updated `SILENT` opcode list in `LoginProtocol::handle()` to use named constants (which now carry RoF2 values) instead of hardcoded Titanium hex literals. `OP_GroundSpawn` (no named const) updated to raw RoF2 value `0x6fca`.

### `src/eq_net/gameplay.rs`
- Updated a comment referencing the old Titanium `OP_APPROVE_WORLD` value (0x3c25 → 0x7499). No code changes.

## Opcode Remapping Count

- **Total application opcodes in protocol.rs**: 110
- **Remapped to RoF2 values**: 100
- **NO-MATCH (left at current value)**: 10

### NO-MATCH Opcodes (10 total)

These have `// RoF2: NO MATCH IN CONF — needs manual resolution` comments in the source.

| Constant | Old Value | Reason |
|---|---|---|
| `OP_SESSION_READY` | 0x0001 | Login-server protocol opcode; `OP_SessionReady=0x0000` in conf |
| `OP_LOGIN` | 0x0002 | Login-server protocol opcode; `OP_Login=0x0000` in conf |
| `OP_SERVER_LIST_REQUEST` | 0x0004 | Login-server protocol opcode; `OP_ServerListRequest=0x0000` in conf |
| `OP_PLAY_EVERQUEST_REQ` | 0x000d | Login-server protocol opcode; `OP_PlayEverquestRequest=0x0000` in conf |
| `OP_CHAT_MESSAGE` | 0x0016 | Login-server protocol opcode; `OP_ChatMessage=0x0000` in conf |
| `OP_LOGIN_ACCEPTED` | 0x0017 | Login-server protocol opcode; `OP_LoginAccepted=0x0000` in conf |
| `OP_SERVER_LIST_RESPONSE` | 0x0018 | Login-server protocol opcode; `OP_ServerListResponse=0x0000` in conf |
| `OP_PLAY_EVERQUEST_RESP` | 0x0021 | Login-server protocol opcode; `OP_PlayEverquestResponse=0x0000` in conf |
| `OP_BECOME_CORPSE` | 0x4dbc | `OP_BecomeCorpse=0x0000` in conf, commented `# Unused?` |
| `OP_LOGOUT_REPLY` | 0x48c2 | `OP_LogoutReply=0x0000` in conf |

**Note on login-server opcodes**: The 8 login-server opcodes (`OP_SESSION_READY` through `OP_PLAY_EVERQUEST_RESP`) are only used in the login-server connection and are not part of the world/zone opcode table. The conf lists them all as `0x0000` because the zone/world patch table does not assign them. These will need separate investigation against the RoF2 login server protocol (likely a later phase or separate task).

**Note on OP_INTERRUPT_CAST**: Previously 0x0000 with comment "native/pass-through value". The RoF2 conf has `OP_InterruptCast=0x048c`, so this was correctly remapped to 0x048c.

## Handshake Struct Confirmation

| Struct | Expected | Actual |
|---|---|---|
| `LoginInfo_S` / `SIZE_LOGIN_INFO` | 464 bytes | 464 bytes ✓ |
| `ClientZoneEntry_S` / `SIZE_CLIENT_ZONE_ENTRY` | 76 bytes | 76 bytes ✓ |

RoF2 `ClientZoneEntry_Struct` (from `rof2_structs.h`):
- `unknown00: u32` (4 bytes, offset 0)
- `char_name: [u8; 64]` (64 bytes, offset 4)
- `unknown68: u32` (4 bytes, offset 68)
- `unknown72: u32` (4 bytes, offset 72)
- Total: 76 bytes

The `on_zone_connected` builder in `login.rs` allocates `vec![0u8; SIZE_CLIENT_ZONE_ENTRY]` (76 bytes, all zeros) and writes the character name at offset 4 — so `unknown00`, `unknown68`, `unknown72` are all zeroed automatically. Same pattern in `gameplay.rs`.

RoF2 `LoginInfo_Struct` (from `rof2_structs.h`):
- `login_info[64]` at offset 0 ✓
- `zoning` at offset 188 ✓
- Total: 464 bytes ✓

No changes required for LoginInfo.

## Build + Test Output

```
Finished `release` profile [optimized] target(s) in 1m 55s
Finished `test` profile [unoptimized + debuginfo] target(s) in 54.02s

running 268 tests
... (4 new RoF2 tests all pass) ...
test result: ok. 250 passed; 0 failed; 18 ignored; 0 measured; 0 filtered out; finished in 0.03s
```

Build: **CLEAN** (no errors, no new warnings).
Tests: **250 passed, 0 failed, 18 ignored**.

### New RoF2 Tests
All 4 new tests pass:
- `eq_net::protocol::tests::rof2_handshake_opcodes_match_conf` ✓
- `eq_net::protocol::tests::rof2_client_zone_entry_size` ✓
- `eq_net::protocol::tests::rof2_login_info_size` ✓
- `eq_net::protocol::tests::rof2_zone_entry_builder_writes_name_at_offset_4` ✓
