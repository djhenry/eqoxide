# Friends list + OP_FriendsWho (RoF2)

## Opcode

`OP_FriendsWho = 0x3956` — confirmed in
`EQEmu/utils/patches/patch_RoF2.conf:229`.

## Client -> Server: NOT a fixed struct — raw NUL-terminated comma string

There is **no `DECODE(OP_FriendsWho)` entry in `common/patches/rof2.cpp`** (grep
returns nothing). That means the RoF2 `Strategy` has no registered ENCODE/DECODE
for this opcode at all — no `DECODE_LENGTH_EXACT` gate, no struct translation.
The packet passes through byte-for-byte from client to
`Client::Handle_OP_FriendsWho`:

```cpp
// EQEmu/zone/client_packet.cpp:6515
void Client::Handle_OP_FriendsWho(const EQApplicationPacket *app)
{
    char *FriendsString = (char*)app->pBuffer;
    FriendsWho(FriendsString);
    return;
}
```

The **entire packet body is read directly as a C string** — no header, no
FromID, no FromName on the wire. Whatever the caller believed (a
`ServerFriendsWho_Struct{FromID; FromName[64]; FriendsString[]}` on the
client->server wire) is **wrong** — that struct exists, but it's the
**zone->world internal servertalk struct** (`ServerOP_FriendsWho = 0x0211`,
`EQEmu/common/servertalk.h:808`), built entirely server-side:

```cpp
// EQEmu/zone/client.cpp:2194-2204 (Client::FriendsWho)
ServerPacket* pack = new ServerPacket(ServerOP_FriendsWho,
        sizeof(ServerFriendsWho_Struct) + strlen(FriendsString));
ServerFriendsWho_Struct* FriendsWho = (ServerFriendsWho_Struct*) pack->pBuffer;
FriendsWho->FromID = GetID();           // filled from the connected zone::Client, not the packet
strcpy(FriendsWho->FromName, GetName()); // ditto
strcpy(FriendsWho->FriendsString, FriendsString);
```

So: **client -> zone wire payload for `OP_FriendsWho` is just the
comma-separated friends string, NUL-terminated, no fixed length, no
DECODE_LENGTH_EXACT check to worry about.** Format, per
`EQEmu/world/clientlist.cpp:930-935` (world's parser, which is the real
authority on the delimiter):

```
"Name1,Name2,Name3"        // comma-delimited, no trailing comma required
```

Parsing loop (`clientlist.cpp:934-960`) splits on `,` and falls back to `\0`
for the last (or only) name. Each individual name must be **<= 64 chars** —
`if ((Seperator - FriendsPointer) > 64) return;` (`clientlist.cpp:941`)
**silently aborts the whole reply with no error sent to the client** if any
single friend name in the string is too long. An empty friends list (empty
string, i.e. just a single NUL byte) is valid and simply produces zero
`FriendsCLEs`.

## Server -> Client reply: reuses OP_WhoAllResponse verbatim

Confirmed — no distinct friends-response opcode/struct.

1. `ClientList::SendFriendsWho` (`EQEmu/world/clientlist.cpp:921-1062`) builds
   the reply using the **same pre-widened internal layout** as normal
   `/who all` (`WhoAllReturnStruct` + `WhoAllPlayerPart1..4`, all from
   `common/eq_packet_structs.h:3711` / `:3686-3708`, NOT the `structs::`
   per-patch versions), and ships it to zone over
   `ServerOP_WhoAllReply` (same internal opcode `/who all` uses,
   `clientlist.cpp:967`).
2. Zone (`EQEmu/zone/worldserver.cpp:471-497`) takes that
   `ServerOP_WhoAllReply` payload, wraps it **unmodified** in an
   `EQApplicationPacket(OP_WhoAllResponse, pack->size)`
   (`worldserver.cpp:483-484`) and `QueuePacket`s it to the requesting client
   — the *same* client identified by `wars->id` (== the FromID the zone
   client filled in from its own `GetID()`, not anything the requester sent).
3. `QueuePacket` runs the packet through the RoF2 stream's registered
   `ENCODE(OP_WhoAllResponse)` (`common/patches/rof2.cpp:4458-4519`), which
   widens each player record by exactly one extra `uint32` (inserts a
   zeroed pad + hardcodes `PIDMSGID=0xFFFFFFFF`) — this is the **same**
   transform a normal `/who all` response goes through.

**Conclusion: the FriendsWho reply is byte-identical in shape, after ENCODE,
to a `/who all` reply — same opcode (`OP_WhoAllResponse`), same
`WhoAllReturnStruct` header (64 bytes, online-count at offset 44), same
widened per-record layout.** eqoxide's existing `apply_who_all` parser
(`src/eq_net/packet_handler.rs:1429`) needs **zero changes** to consume it —
confirmed by tracing the header comment already correctly cites
`ENCODE(OP_WhoAllResponse)` and matches field-for-field
(`FormatMSGID | pad0 | PIDMSGID | Name | RankMSGID | Guild | Unknown80×2 |
ZoneMSGID | Zone | Class | Level | Race | Account | Unknown100`).

Only behavioral difference from a real `/who all`: `playercount` /
`unknown44[0]` == number of *online friends found*, not zone/server
population, and the header's `id` field is echoed back as the requester's own
spawn id (harmless — `apply_who_all` never reads it).

## Anonymous / roleplay filtering

`clientlist.cpp:947`: a friend is included in `FriendsCLEs` only if

```cpp
CLE && CLE->name() && (CLE->Online() >= CLE_Status::Zoning)
      && !(CLE->GetGM() && CLE->Anon())
```

So: **anonymous non-GM friends ARE included** (unlike a filtered `/who`
search by name, which anon players can dodge) — only an **anonymous GM** is
excluded outright. For everyone else who *is* included, anonymity still
degrades the per-record payload the same way `/who all` does
(`clientlist.cpp:1001-1018`): `Anon()==1` -> `FormatMSGID=5024` ("[ANONYMOUS]
Name"), `Anon()==2` -> `5023`, and class/level/race/zone are zeroed with
`ZoneMSGID=0xFFFFFFFF` when `Anon()!=0` — this is exactly the sentinel
`apply_who_all` already treats as anonymous (`anon = zonestr==0xFFFF_FFFF ||
(class==0 && level==0 && race==0)`), so anonymous friends surface with a
name but no class/level/zone, same as `/who all`.

Offline friends are simply **absent** from the reply — there is no
"offline" marker; the caller's belief ("online set is these names; offline
friends are just absent") is confirmed correct.

## Builder spec for eqoxide (SEND direction)

For `OP_FriendsWho`, do **not** build a struct with FromID/FromName — just
send the raw string:

```
payload = "<friend1>,<friend2>,...,<friendN>\0"
```

- No packet header, no length-prefix field — RoF2 has no DECODE handler for
  this opcode, so whatever bytes eqoxide sends arrive unmodified at
  `Handle_OP_FriendsWho`.
- **Must be NUL-terminated** — the server casts `app->pBuffer` directly to
  `char*` and calls `strchr`/`strncpy` on it with no length bound from
  `app->size`; a non-NUL-terminated buffer risks an OOB read server-side
  (not eqoxide's problem to fix, but terminate defensively — cheap and
  correct).
- Comma-separated, **no spaces**, case handling is server-side
  (`FindCharacter` presumably case-insensitive — not verified here).
- Each name must be <= 64 bytes or the *entire* reply is dropped
  server-side with no error — client-side validation (reject/warn on
  friend names > 64 chars before sending) is cheap insurance.
- Empty friends list -> send a single `\0` byte (or just omit the opcode
  send if there's nothing to query — no requirement to always poll).
- No trailing pad, no byte-order concerns (it's a string, not a binary
  struct).

## Reply parsing (already correct)

Route `OP_FriendsWho`'s reply through the **existing** `apply_who_all`
parser at `src/eq_net/packet_handler.rs:1429` (dispatched today only from
`OP_WHO_ALL_RESPONSE` at `packet_handler.rs:115` — add `OP_FriendsWho`'s
reply, which is *also* opcode `OP_WhoAllResponse` on the wire, so **no new
opcode constant or parser is needed**, only a way to distinguish "this
`OP_WhoAllResponse` was solicited by a friends poll vs a `/who all`" if the UI
needs to route results differently — the wire itself carries no such
discriminator; eqoxide must track that client-side by request intent (e.g. a
pending-request flag set when `OP_FriendsWho` is sent, cleared on next
`OP_WhoAllResponse`).

## Client-local friends storage (corroboration, not wire-related)

The RoF2 client's local friends list is confirmed to persist as
per-character INI keys (`decompiled/ghidra/eqgame.exe.c:169225-169232`):
section `[Friends]`, keys `Friend0=Name`, `Friend1=Name`, ... — consistent
with issue #301's framing that friends are client-local and presence is a
separate pull. (Not directly load-bearing for the wire protocol — eqoxide's
own local friends storage format doesn't need to match this INI convention
unless parity with the retail INI file is a goal.)

## Status

Confirmed directly from EQEmu RoF2 source for both directions (no
Ghidra confirmation of the exact client-side OP_FriendsWho send path was
attempted beyond the INI corroboration above — EQEmu server behavior is
authoritative enough here since eqoxide only needs to match what the *server*
accepts/produces, not replicate original client internals).
