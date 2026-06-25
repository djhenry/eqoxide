# Logout / Disconnect / Reconnect Protocol — Titanium / EQEmu

## Opcode Reference (Titanium wire values)

| Symbolic name        | Hex value | Direction       | Notes |
|----------------------|-----------|-----------------|-------|
| OP_Camp              | 0x78c1    | client→zone     | Triggers 30-second camp countdown on server |
| OP_Logout            | 0x61ff    | client→zone     | Sent at countdown expiry (camp complete) OR "quit to desktop" shortcut |
| OP_LogoutReply       | 0x48c2    | zone→client     | Server ack; client transitions to char-select screen |
| OP_PreLogoutReply    | (see below)| zone→client    | Sent before LogoutReply in SendLogoutPackets(); Titanium value appears unmapped (0x0000 in all extractor confs; treated as no-op by client) |
| OP_CancelTrade       | 0x2dc1    | zone→client     | Sent with LogoutReply by SendLogoutPackets() |
| OP_WorldLogout       | 0x7718    | client→world    | Sent after "quit to desktop" completes, at the world-server connection |
| OP_GMKick            | 0x692c    | zone→client     | Used to boot a duplicate/kicked session |
| OP_SessionDisconnect | 0x05      | either          | EQStream session-layer teardown, not an application opcode |

Sources:
- EQEmu's [`utils/patches/opcodes.conf`](https://github.com/EQEmu/Server/blob/master/utils/patches/opcodes.conf) (opcode → name mapping)
- Observed behavior of the original Titanium game client (`eqgame.exe`): which
  opcodes it sends/receives during logout —
  - 0x78c1 sent
  - 0x61ff sent
  - 0x7718 sent
  - 0x48c2 received
  - 0x692c received

---

## Q1 — Clean Camp Sequence

### Camp to character select ("30-second camp")

**Client sends:**
1. `OP_Camp (0x78c1)` — 4-byte payload (a small fixed struct; exact contents unimportant for the server; zero-fill is accepted). This triggers `camp_timer.Start(29000)` on the server.
2. After 30 seconds, with the countdown running in the client's process loop, client sends `OP_Logout (0x61ff)` — zero-length payload.
3. The client then blocks (up to ~3 s) waiting for the stream to drain before moving to char-select.

**Server response (zone side):**
- On `Handle_OP_Camp`: starts `camp_timer` for 29 000 ms (`client_packet.cpp:4294`).
- On `camp_timer.Check()` firing (`client_process.cpp:193–212`): saves char, sets `instalog = true`, disables the timer. `instalog = true` is the flag that tells the disconnect handler to skip the linkdead path.
- On `Handle_OP_Logout` (`client_packet.cpp:10265`): calls `SendLogoutPackets()` (sends `OP_CancelTrade` + `OP_PreLogoutReply`), then sends `OP_LogoutReply (0x48c2)`, then calls `Disconnect()`.
- When EQStream closes (after `Disconnect()`), `client_process.cpp:626–631` detects disconnect. Because `instalog == true`, it calls `OnDisconnect(false)` (no hard disconnect) and returns `false` — client entity is cleanly removed, **no linkdead state**.

**Quit to desktop** (bypass camp countdown):
- Client sends `OP_Logout (0x61ff)` directly (no OP_Camp first). At the world-server connection it sends `OP_WorldLogout (0x7718)`.
- On the zone side, if the EQStream closes abruptly (no OP_Logout received), the process drops into the linkdead path.
- On the world side, `OP_WorldLogout` causes `eqs->Close()` + `cle->SetOnline(CLE_Status::Offline)` (`world/client.cpp:1177–1182`). Comment in source: "I don't see this getting executed on logout" — implies it is often not sent or received.

**Session layer (EQStream):**
- After sending app-layer `OP_Logout`, the zone sends `OP_SessionDisconnect (0x05)` at the EQStream layer as part of `Disconnect()`. The payload is: `[0x00][0x05][connect_code u32]` (8 bytes with CRC). Source: `common/net/reliable_stream_connection.cpp:1333–1342` and `reliable_stream_structs.h:89–103`.
- The client does NOT need to send `OP_SessionDisconnect` first; the server sends it in response.

**Minimal correct sequence for Rust client "camp to char select":**
1. Send `OP_Camp (0x78c1)` with 4-byte payload (zero-fill ok).
2. Wait 30 seconds (or show countdown UI).
3. Send `OP_Logout (0x61ff)` with 0-byte payload.
4. Wait for `OP_LogoutReply (0x48c2)` from server (or timeout ~3 s).
5. Close EQStream connection (send `OP_SessionDisconnect 0x05`).

**Minimal correct sequence for "quit to desktop":**
- Same as above (send OP_Camp + wait + OP_Logout) for a clean exit. If bypassing the countdown, send `OP_Logout` alone and then close the EQStream. The server will enter the linkdead path briefly but `OnDisconnect` still saves the character.

---

## Q2 — Orphaned Session / Reconnect

When a client logs back in for a character that is still in a zone (linkdead state), the world server's `EnterWorld()` calls:

```
ZSList::Instance()->DropClient(GetLSID(), zone_server);
```
(`world/client.cpp:1429`)

`ZSList::DropClient` broadcasts a `ServerOP_DropClient` packet to **all zone servers except the new destination zone** (`world/zonelist.cpp:841–851`). The zone receiving it:
- Calls `zone->RemoveAuth(drop->lsid)`.
- Finds the ghost client by LSID: `entity_list.GetClientByLSID(drop->lsid)`.
- Calls `client->Kick("Dropped by world CLE subsystem")` and `client->Save()` (`zone/worldserver.cpp:649–663`).

`Kick()` sets `client_state = CLIENT_KICKED`. On next process tick, `client_process.cpp:563–568` catches this: calls `Save()` then `OnDisconnect(true)` — clean removal, no linkdead timer started.

**Result:** On reconnect, the orphaned session is immediately kicked and removed; the new client takes over. **There is no need to wait for the linkdead timeout.** The world server handles this automatically as soon as `EnterWorld()` is processed.

**The "force take-over" mechanism IS `DropClient`** — it is automatic. The new client's `HandleEnterWorldPacket` calls `EnterWorld()` which calls `DropClient` before telling the zone to accept the new connection.

---

## Q3 — Double Login (Two Live Clients)

When a second client logs in with the same LS account, two things happen:

1. **IP limit check** (`world/clientlist.cpp` around line 195–216): if `RuleI(World, MaxClientsPerIP)` is set and the IP matches an existing live session, the existing session is kicked via `ServerOP_KickPlayer` → `WorldKick()` on the zone. The booted client receives `OP_GMKick (0x692c)` (`zone/client.cpp:3329–3336`).

2. **DropClient** (same as orphan case): even if IP limits are not hit, `ZSList::DropClient` still broadcasts to evict the old zone client.

**What the booted client sees:** `OP_GMKick (0x692c)` — a `GMKick_Struct` payload containing the character's name. The client sets its internal disconnect flags and immediately begins the quit path.

**Distinguishing live from linkdead:** EQEmu does not distinguish them differently for the DropClient path — both are evicted the same way. A live session will receive the `OP_GMKick`; an already-dead session (linkdead timer running) will just get `Kick()` set on the ghost and it will be cleaned up on next process tick.

---

## Q4 — Linkdead Timeout

**Default:** `90 000 ms` (90 seconds).  
**Rule name:** `Zone:ClientLinkdeadMS`  
**Source:** EQEmu's [`common/ruletypes.h`](https://github.com/EQEmu/Server/blob/master/common/ruletypes.h) (`Zone:ClientLinkdeadMS`)

```
RULE_INT(Zone, ClientLinkdeadMS, 90000,
  "The time a client remains link dead on the server after a sudden disconnection (milliseconds)")
```

There is also a secondary `CLIENT_LD_TIMEOUT = 30000` (`zone/client.h:83`) used for `AI_Start()` — this controls how long the NPC AI waits before forgetting the linkdead client for aggro purposes. It is **not** the character removal timer.

During the 90-second window the character remains in-world as a linkdead NPC (AI-controlled, no movement). At expiry `client_process.cpp:159–184` fires: saves char, removes from guild, removes from raid, and returns `false` to delete the entity.

---

## Recommendation for eqoxide

### On clean exit / ctrl-c / SIGTERM:

```
1. Send OP_Camp (0x78c1, 4-byte zero payload)   // triggers server-side 29s timer + instalog flag
2. Wait 30 000 ms (or implement countdown UI)
3. Send OP_Logout (0x61ff, 0-byte payload)       // server calls SendLogoutPackets → LogoutReply
4. Wait for OP_LogoutReply (0x48c2) up to 3 s
5. Send OP_SessionDisconnect (0x05) at stream layer, close socket
```

If the user wants instant exit (no 30s wait), send only `OP_Logout` (no OP_Camp) — the server saves the character but enters the linkdead path for ~90 s before full cleanup. Character position/state IS saved.

### On reconnect after crash:
No special handling needed. Just connect normally: the world server's `EnterWorld()` automatically calls `DropClient` which kicks any orphaned zone session before the new connection lands. The only concern is the ~1–2 second propagation delay for `DropClient` to reach the old zone and for the zone to boot up and accept the new client — the world code handles this with `IncomingClient()` / `Clearance()`.

### Receiving OP_GMKick (0x692c):
The Rust client should handle this. It means the zone server has booted our session (due to duplicate login or admin kick). Correct response: display a "You have been disconnected" message, then close the connection cleanly (send `OP_SessionDisconnect`). The `GMKick_Struct` payload is just a `char name[64]` (confirmed from `zone/client.cpp:3332`).

### Linkdead tolerance:
If the Rust process dies without sending OP_Logout, character is in-world linkdead for 90 seconds (configurable). Position and inventory **are** saved at `client_process.cpp:161` when the linkdead timer fires. This is safe if infrequent.
