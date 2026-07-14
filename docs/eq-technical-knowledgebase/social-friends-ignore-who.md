# Social: Friends (buddy) list, Ignore list, /who and /who all (RoF2)

Status: confirmed (server side fully; client side confirmed via decompile +
live UI/ini artifacts, exact network-send call sites not traced instruction-
by-instruction — see "client-side" notes below for what is inferred).

## 1. Friends / buddy list — NO server-side persistence, NO push notification

**Opcode:** `OP_FriendsWho = 0x3956` (client -> server), one-shot request.
`EQEmu/utils/patches/patch_RoF2.conf:229`

**There is no dedicated add/remove/list opcode.** The friends list itself is
**stored entirely client-side**, in the per-character ini file
(`<CharName>_<ServerName>.ini`, e.g. `Brusk_EQ Reborn.ini:22` in the RoF2
install), under a `[Friends]` section:
```
[Friends]
SendToUChat=0
Friend0=SomeName
Friend1=OtherName
...
```
(`Friend0`..`Friend99`, keys read via `FUN_00861120("Friends", "Friend%d", ...)`
in the client — `everquest_rof2/decompiled/ghidra/eqgame.exe.c:169231-169233`.)

**Add/remove is a local console-command round-trip, not a network opcode.**
The Friends window's Add/Delete buttons (`EQUI_FriendsWnd.xml:25-46,94-115`,
ScreenIDs `AddButton`/`DeleteButton`) synthesize and run the commands
`"buddy %s"` (add) / `"buddy -%s"` (remove) through the client's own
command-line processor (`eqgame.exe.c:150159` / `:150150`, string literals
confirmed in `decompiled/capstone/eqgame.exe.asm:320777,320864`). This updates
the client's in-memory/ini friends list only; nothing is sent to the server at
add/remove time.

**"Who of my friends is online" is a manual pull, not a push.** The Friends
window has a `WhoButton` (`EQUI_FriendsWnd.xml:71-92`, ScreenID `WhoButton`) —
clicking it (or, per the decompile, on zoning into world the client replays
its whole locally-stored friends+ignored list back into the command
processor via `FUN_00517d50`, `eqgame.exe.c:169206-169254`) causes the client
to send **the full comma-separated name list** in one `OP_FriendsWho` packet:
```c
// EQEmu/zone/client.cpp:2194-2208
void Client::FriendsWho(char *FriendsString) {
    // ServerFriendsWho_Struct{ FromID; FromName[64]; FriendsString[1] /*CSV*/ }
    // -> ServerOP_FriendsWho to world
}
```
`ServerFriendsWho_Struct`: `EQEmu/common/servertalk.h:808-812`.

World (`ClientList::SendFriendsWho`, `EQEmu/world/clientlist.cpp:921-1062`)
parses the CSV, looks up each name in the online client list, and — for
those currently online (`CLE->Online() >= CLE_Status::Zoning`) — builds a
response that **reuses the /who-all wire format verbatim**:
```c
auto pack2 = new ServerPacket(ServerOP_WhoAllReply, ...);   // clientlist.cpp:967
```
which the zone server converts straight into `OP_WhoAllResponse` back to the
client (`EQEmu/zone/worldserver.cpp:471-495`). So **the friends list "is
online" check comes back as an ordinary OP_WhoAllResponse**, just filtered
down to only your friends that are logged in — same struct/opcode as regular
/who (see section 3). Offline friends simply don't appear in the reply; there
is no explicit "X went offline" message and no unsolicited/pushed packet at
all for friend state changes. Anonymous friends are still suppressed per the
usual /who anon rules (`Anon()==1/2` -> hides class/level/race/zone,
`clientlist.cpp:1001-1018`), and a friend who is a hidden GM (`CLE->GetGM() &&
CLE->Anon()`) is dropped entirely from the reply (`clientlist.cpp:947`).

**Recommendation for eqoxide:** don't build a friends *feature* around a
server push — there isn't one. To replicate:
- Keep the friends list as client-local config (a simple list of names),
  matching RoF2's ini-based model — no server round trip needed to "add" a
  friend.
- To answer "is X currently online," send `OP_FriendsWho` (0x3956) with a
  NUL/CSV-joined name string (match `ServerFriendsWho_Struct` semantics:
  server just does `strchr(...,',')`/`strchr(...,'\0')` tokenizing, so a
  single name with no trailing comma also works) and parse the reply as a
  normal `OP_WhoAllResponse` (see section 3's RoF2-specific wire format —
  it is NOT the generic/common WhoAllResponse layout).
- If eqoxide wants live "friend came online" notifications for an AI agent,
  that has to be **synthesized client-side by polling** (re-send
  `OP_FriendsWho` on an interval and diff the returned name set against the
  previous poll) — this is exactly what the real client does on a timer/UI
  action, there's no cheaper wire-level shortcut available.

## 2. Ignore list — 100% client-side, server does nothing with it

No opcode is genuinely named/implemented for ignore add/remove in the RoF2
patch. The RoF2 client_packet.cpp table binds **two placeholder/"Unknown"
opcodes** to a stub handler that does nothing:
```c
// EQEmu/zone/client_packet.cpp:277,411
ConnectedOpcodes[OP_ItemViewUnknown] = &Client::Handle_OP_Ignore;   // 0x465b, patch_RoF2.conf:339
ConnectedOpcodes[OP_WorldUnknown001] = &Client::Handle_OP_Ignore;   // 0x2301, patch_RoF2.conf:45
```
```c
// EQEmu/zone/client_packet.cpp:8975-8978
void Client::Handle_OP_Ignore(const EQApplicationPacket *app) { return; }
```
i.e. whatever the client sends when you add/remove someone from Ignore is
received and silently discarded — the server never stores or enforces an
ignore list. (These opcode *names* are themselves EQEmu placeholders — RoF2's
real "ignore" wire opcode has not been identified/named in this patch; it's
just mapped to a no-op so the unknown-opcode warning log doesn't fire.)

Like Friends, the list is persisted client-side in the per-character ini
under `[Ignored]` (`Ignored0`..`Ignored99`,
`eqgame.exe.c:169238-169239,169241-169244`), rebuilt at zone-in via the
same `FUN_00517d50` replay loop using the command pair `"ignoreplayer %s"` /
`"ignoreplayer -%s"` (string literals confirmed at
`decompiled/capstone/eqgame.exe.asm:325887,325933,371067`). Ignore filtering
(tells/chat suppression) therefore happens **entirely client-side** against
that local list; the server has no concept of it.

`EQUI_FriendsWnd.xml` also exposes a third, separate list — `MutedList` /
`FW_MutedDeleteButton` (`EQUI_FriendsWnd.xml:265-287,290`) — distinct from
Ignore (likely a proximity-voice/text mute list). Not investigated further;
flag if a feature request needs it.

**Recommendation for eqoxide:** implement Ignore purely as client/agent-side
state (a name set the HUD/agent consults before surfacing tell/say events).
There is nothing to send to the RoF2 server; do not build an
add/remove/list opcode flow for it.

## 3. /who and /who all

**Request:** `OP_WhoAllRequest = 0x674b` (client -> server),
`patch_RoF2.conf:227`. Payload is **RoF2-specific and wider than other
patches**:
```c
// EQEmu/common/patches/rof2_structs.h:2757-2768  (156 bytes total)
struct Who_All_Struct {
    char   whom[64];
    uint8  unknown088[64];   // <-- RoF2-only 64-byte pad, not present in
                              //     the generic/SoF struct (76 bytes total,
                              //     eq_packet_structs.h:2651-2663)
    uint32 wrace;      // 0xFFFFFFFF = no filter
    uint32 wclass;     // 0xFFFFFFFF = no filter
    uint32 lvllow;     // 0xFFFFFFFF = no filter
    uint32 lvlhigh;    // 0xFFFFFFFF = no filter
    uint32 gmlookup;   // 0xFFFFFFFF = not doing /who all gm
    uint32 guildid;    // 0xFFFFFFFF=none, 0xFFFFFFFC=trader-only,
                        // 0xFFFFFFFB=buyer-only (also LFG), else exact guild id
    uint32 type;       // 0 = /who (zone-local), 3 = /who all (server-wide)
};
```
`DECODE_LENGTH_EXACT` is enforced (`rof2.cpp:6373-6388`) — an eqoxide
implementation MUST send exactly 156 bytes with the extra `unknown088[64]`
pad, or the RoF2-patched server drops the packet. `type==0` routes zone-local
(`EntityList::ZoneWho`, `zone/entity.cpp:4866-4938`, called from
`Client::Handle_OP_WhoAllRequest`, `zone/client_packet.cpp:16122-16135`);
any other type goes to world for a server-wide scan
(`Client::WhoAll` -> `ServerOP_Who` -> `ClientList::SendWhoAll`,
`zone/client.cpp:2173-2192`, `world/clientlist.cpp:601`).

**Response:** `OP_WhoAllResponse = 0x578c`, `patch_RoF2.conf:228`.
Header: `WhoAllReturnStruct` (`eq_packet_structs.h:3711-3723`, same layout
used internally by both zone-local and world-relayed paths) followed by a
variable-length player array. **RoF2 has its own ENCODE that reshuffles the
header and widens every player record by 4 bytes** vs. the generic encode
other patches use:
```c
// EQEmu/common/patches/rof2.cpp:4458-4520
ENCODE(OP_WhoAllResponse) {
    wars->unknown44[0] = Count;   // RoF2 moves the online-count here
    wars->unknown52   = 0;        // and zeroes what used to hold it
    // per player record, RoF2 inserts one extra always-zero uint32
    // between FormatMSGID and the (still hardcoded 0xffffffff) PIDMSGID:
    //   FormatMSGID(u32), 0(u32, RoF2-only pad), PIDMSGID=0xFFFFFFFF(u32),
    //   Name(cstr), RankMSGID(u32), Guild(cstr, "" if none),
    //   Unknown80[2](u32 x2, 0xFFFFFFFF,0xFFFFFFFF), ZoneMSGID(u32),
    //   Zone(u32), Class(u32), Level(u32), Race(u32),
    //   Account(cstr, empty unless privileged), Unknown100(u32, =207)
}
```
FormatMSGID selects the client-localized who-line format string (via
`db_str`/eqstr ids): `5025` normal, `5024`/`5023` anonymous variants
(`world/clientlist.cpp:1000-1004`); when `Anon()!=0` the class/level/
race/zone fields are zeroed and `ZoneMSGID` is set to `0xFFFFFFFF`
(`clientlist.cpp:1009-1018`) so the client renders "ANONYMOUS" instead of
stats/location. Player counts: `5028` singular / `5036` plural / `5029` zero
(`clientlist.cpp:980-983`, `entity.cpp:4922-4931`).

**Recommendation for eqoxide:** parse RoF2 `OP_WhoAllResponse` using the
reshuffled/widened layout above — reusing the plain
`eq_packet_structs.h` `WhoAllReturnStruct`/`WhoAllPlayer` field order verbatim
will misalign every record by 4 bytes after the first. For "who is online"
tooling (server-wide /who all for agent tooling), send `OP_WhoAllRequest`
with `type=3`, all filter fields `0xFFFFFFFF`, `whom=""`; for a zone roster
use `type=0`. Treat `guild==""` / `zone==0xFFFFFFFF`(ZoneMSGID) /
`class==level==race==0` as the "hidden due to Anon" case, not as real zeroed
data.

## Cross-reference
- Chat/tell routing: see `eq-chat-wire-format-and-routing.md`.
- RoF2 PlayerProfile / character-scoped persisted client state generally:
  see `eq-rof2-playerprofile-streamed.md`.
