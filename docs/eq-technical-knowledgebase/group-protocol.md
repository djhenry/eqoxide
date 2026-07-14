# Group protocol (RoF2)

Status: all findings below are **confirmed** against the open-source EQEmu server
([github.com/EQEmu/Server](https://github.com/EQEmu/Server) — RoF2 patch files and zone server
logic) unless marked "inferred". Client message text is resolved via server-sent `MessageString`
IDs rather than being fixed client-side, so client-side rendering behavior below is inferred from
the server's packet-construction comments rather than observed directly.

Related: `hp-update.md` (group unlocks percent HP/mana/endurance for other members, NOT full
`OP_HPUpdate` — corrected there).

## 1. Opcode list (RoF2 wire values)

Source: `EQEmu/utils/patches/patch_RoF2.conf:533-552`.

| App opcode | Hex (RoF2 wire) | Direction | Length |
|---|---|---|---|
| `OP_GroupInvite` | `0x6110` | C→S (send invite) and S→C (deliver invite to invitee) | fixed, 148B (RoF2 `GroupInvite_Struct`) |
| `OP_GroupInvite2` | `0x32c2` | C→S (invite while target-required rule variant) | same struct as GroupInvite; server `DECODE_FORWARD`s it to the `OP_GroupInvite` decoder (`rof2.cpp:5696-5700`) |
| `OP_GroupFollow` | `0x1649` | C→S (accept invite) / S→C (relay accept to inviter) | fixed, 152B (RoF2 `GroupFollow_Struct`) |
| `OP_GroupFollow2` | `0x2060` | same as GroupFollow; `Client::Handle_OP_GroupFollow` just calls `Handle_OP_GroupFollow2` (`zone/client_packet.cpp:7377-7380`) | fixed, 152B |
| `OP_GroupCancelInvite` | `0x0000` **(disabled/not sent standalone in RoF2 conf — see note below)** | C→S (decline) / S→C (relay decline to inviter) | fixed, 152B (RoF2 `GroupCancel_Struct`) |
| `OP_GroupDisband` | `0x4c10` | C→S (leave/kick request) | fixed, 148B (common `GroupGeneric_Struct`; RoF2 has no override so common struct size/layout applies) |
| `OP_GroupDelete` | `0x0f6c` | C→S ("disband whole group" button, no payload used) | `Handle_OP_GroupDelete` ignores payload, just calls `group->DisbandGroup()` (`zone/client_packet.cpp:7182-7193`) |
| `OP_GroupUpdate` | `0x3abb` | S→C, internal/server-side app opcode that fans out to several *different real wire opcodes* depending on `action` (see §2) | **variable** — see below |
| `OP_GroupUpdateB` | `0x6194` | S→C only — the actual wire opcode used for full-roster pushes | **variable length**, built field-by-field with `VARSTRUCT_ENCODE_*` (streamed, like `OP_PlayerProfile`) |
| `OP_GroupDisbandYou` | `0x1ae5` | S→C (you left/were removed) | fixed, 148B `GroupGeneric_Struct` |
| `OP_GroupDisbandOther` | `0x74da` | S→C (someone else left/was removed from your group) | fixed, 148B `GroupGeneric_Struct` |
| `OP_GroupAcknowledge` | `0x7323` | C→S (no-op ack, ignored server-side) / S→C (4-byte "you joined the group" popup trigger) | C→S: ignored (`Handle_OP_GroupAcknowledge` returns immediately, `zone/client_packet.cpp:7146-7149`); S→C: 4 bytes, no payload struct (`Client::SendGroupJoinAcknowledge`, `zone/client.cpp:~6708`) |
| `OP_GroupMakeLeader` | `0x4229` | C→S (`/makeleader`) | fixed, 456B `GroupMakeLeader_Struct` (`common/eq_packet_structs.h:5860-5867`) |
| `OP_GroupLeaderChange` | `0x21b4` | S→C (push new leader name to every member, SoD+/RoF2 only) | fixed, 148B `GroupLeaderChange_Struct` — **no RoF2 override in `rof2.cpp`**, passes through as the common struct unmodified |
| `OP_GroupRoles` | `0x70e2` | C→S (toggle main tank / main assist / puller delegation) | fixed, 148B `GroupRole_Struct` |
| `OP_GroupMentor` | `0x3342` | C→S/S→C (group mentoring %) | fixed, 68B `GroupMentor_Struct` |
| `OP_GroupLeadershipAAUpdate` | `0x02cf` | S→C (leader ability AA state push) | fixed, `GroupLeadershipAAUpdate_Struct` (320B, `common/eq_packet_structs.h:2526-2534`) |
| `OP_DoGroupLeadershipAbility` | `0x6eae` | C→S (use a leadership AA, e.g. Mark NPC) | fixed, `DoGroupLeadershipAbility_Struct` |
| `OP_SetGroupTarget` | `0x2814` | C→S | not investigated in depth for this report |
| `OP_AssistGroup` | `0x27f8` | C→S (`/assist` broadcast to group) | not investigated in depth |
| `OP_Some6ByteHPUpdate` | `0x0000` | comment: "seems to happen when you target group members" — disabled/placeholder in RoF2 conf, not used | n/a |

**Important quirk on `OP_GroupCancelInvite`:** the RoF2 opcode table lists it as `0x0000`
(`patch_RoF2.conf:539`), same convention used elsewhere in this conf file for "opcode intentionally
not remapped / falls through to a default/disabled value" — cross-check this against a live packet
capture before wiring it up; treat the numeric value here as **unconfirmed for RoF2** even though
the struct layout and encode/decode handlers for it are fully implemented in `rof2.cpp` (i.e. the
server-side code path is real, only the concrete wire hex is in doubt from the conf file alone).

## 2. Wire struct layouts (RoF2, `common/patches/rof2_structs.h`)

RoF2 struct sizes differ from the generic/Titanium ones in `common/eq_packet_structs.h` — always
use the RoF2-namespaced ones below.

### GroupInvite_Struct — 148 bytes (`rof2_structs.h:2565-2574`)
```c
struct GroupInvite_Struct {
/*0000*/ char   invitee_name[64];
/*0064*/ char   inviter_name[64];
/*0128*/ uint32 unknown0128;
/*0132*/ uint32 unknown0132;
/*0136*/ uint32 unknown0136;
/*0140*/ uint32 unknown0140;
/*0144*/ uint32 unknown0144;
/*0148*/
};
```
Encode: `ENCODE(OP_GroupInvite)` (`rof2.cpp:1558-1567`) only fills `invitee_name`/`inviter_name`
from the internal `GroupGeneric_Struct{name1,name2}` (name1→invitee, name2→inviter); the 5 trailing
`uint32` unknowns are zero-filled by `SETUP_DIRECT_ENCODE` (fresh zeroed buffer). Decode mirrors
this (`rof2.cpp:5682-5694`).

### GroupGeneric_Struct — 148 bytes (`rof2_structs.h:2576-2585`)
```c
struct GroupGeneric_Struct {
/*0000*/ char   name1[64];
/*0064*/ char   name2[64];
/*0128*/ uint32 unknown0128;
/*0132*/ uint32 unknown0132;
/*0136*/ uint32 unknown0136;
/*0140*/ uint32 unknown0140;
/*0144*/ uint32 unknown0144;
/*0148*/
};
```
Used on the wire for `OP_GroupDisbandYou`/`OP_GroupDisbandOther`. **CONFIRMED LIVE (task-6
validation, 2026-07-01) against a running EQEmu RoF2 zone server:** `OP_GroupDisband`
(client→server, the leave/kick/decline-cleanup request) also uses this same 148-byte
RoF2-namespaced `GroupGeneric_Struct` (`name1[64]`, `name2[64]`, 5 trailing zero uint32s) — NOT
the common 128-byte struct this doc previously guessed at. Sending a 128-byte payload was
observed to be silently rejected server-side:
```
[Zone] [Netcode] Wrong size on incoming [OP_GroupDisband] (structs::GroupGeneric_Struct): Got [128], expected [148]
[Zone] [Packet C->S] [OP_Unknown] [0x4c10] Size [130]
```
(the extra 2 bytes are the opcode header). The server logged the packet as `OP_Unknown` and
dropped it entirely — no roster change, no error surfaced to the client, no crash. eqoxide's
`build_group_disband()` in `src/eq_net/navigation.rs` was fixed to emit 148 bytes; see
`docs/eq-technical-knowledgebase/... ` task-6 report for the full repro. The earlier theory that
`zone/client_packet.cpp:7197`'s `sizeof(GroupGeneric_Struct)` resolves to the common 128-byte
struct in the zone translation unit is contradicted by this live evidence — either the zone
binary in this build pulls in the RoF2-namespaced struct after all, or a different check path is
in play. Trust the live packet capture over the static-analysis inference here.

### GroupCancel_Struct (decline) — 152 bytes (`rof2_structs.h:2587-2593`)
```c
struct GroupCancel_Struct {
/*000*/ char   name1[64];
/*064*/ char   name2[64];
/*128*/ uint8  unknown128[20];
/*148*/ uint32 toggle;
/*152*/
};
```
`toggle` is **never inspected by the server** — `Client::Handle_OP_GroupCancelInvite`
(`zone/client_packet.cpp:7151-7180`) just forwards the packet verbatim to the inviter (same-zone)
or via `ServerOP_GroupCancelInvite` (cross-zone, `zone/worldserver.cpp:1262-1269`), then
unconditionally calls `Group::RemoveFromGroup(this)`. Semantics of `toggle` (e.g. distinguishing
"declined" vs "invite timed out/cancelled") are client-side only and are not observable from the
server source; treat as **inferred**.

### GroupFollow_Struct (accept) — 152 bytes (`rof2_structs.h:2648-2658`)
```c
struct GroupFollow_Struct { // "Live" follow struct, used by RoF2
/*0000*/ char   name1[64]; // inviter
/*0064*/ char   name2[64]; // invitee
/*0128*/ uint32 unknown0128; // seen 0
/*0132*/ uint32 unknown0132; // group ID or member level?
/*0136*/ uint32 unknown0136; // maybe voice chat channel or group ID?
/*0140*/ uint32 unknown0140; // seen 0
/*0144*/ uint32 unknown0144; // seen 0
/*0148*/ uint32 unknown0148;
/*0152*/
};
```
Both `OP_GroupFollow` and `OP_GroupFollow2` encode/decode to this same struct
(`rof2.cpp:1536-1556` encode, `5654-5680` decode) — direct-mapped from the internal
`GroupGeneric_Struct{name1,name2}` (name1=inviter, name2=invitee), trailing fields zero-filled.

### GroupJoin_Struct — 148 bytes (`rof2_structs.h:2638-2646`)
```c
struct GroupJoin_Struct {
/*000*/ char   owner_name[64]; // merc owner, or just group-relative "your name" slot in practice
/*064*/ char   membername[64];
/*128*/ uint8  merc;
/*129*/ uint8  padding129[3];
/*132*/ uint32 level;
/*136*/ uint8  unknown136[12]; // group ID likely lives here, unused by client
/*148*/
};
```
This is the wire struct for `action ∈ {groupActJoin(0), groupActMakeLeader(8), groupActAAUpdate(10)}`
sent under app-opcode `OP_GroupUpdate` — encoded via `ENCODE_LENGTH_EXACT(GroupJoin_Struct)` /
`SETUP_DIRECT_ENCODE(GroupJoin_Struct, structs::GroupJoin_Struct)` (`rof2.cpp:1700-1714`), which
only copies `membername` and separately builds/dispatches a second `OP_GroupLeadershipAAUpdate`
packet — i.e. **every `OP_GroupUpdate` (join/makeleader/AA-update variants) also triggers an
immediate second packet**, `OP_GroupLeadershipAAUpdate`, carrying `NPCMarkerID` + leader AA state.

### GroupUpdate_Struct (452B) / GroupUpdate2_Struct (768B) — internal server structs, NOT sent as-is on RoF2 wire
`rof2_structs.h:2595-2611`. These are what `zone/groups.cpp` builds internally
(`Group::DisbandGroup`, `Group::SendUpdate`) as app-layer `OP_GroupUpdate` packets, but RoF2's
`ENCODE(OP_GroupUpdate)` (`rof2.cpp:1569-1717`) inspects `action`/`in->size` and **rewrites them
into different real wire opcodes** — see §3 table. **Neither of these fixed structs actually hits
the RoF2 wire** in their raw form except as the intermediate/server-internal representation.

### OP_GroupUpdateB — the real full-roster packet, variable length (streamed)

**This is the group-protocol analogue of the `OP_PlayerProfile` streaming quirk** — there is no
fixed struct; it's built byte-by-byte with `VARSTRUCT_ENCODE_*` macros in three places that must
agree: `rof2.cpp:1614-1697` (`action==groupActUpdate`/`GroupUpdate2_Struct`-sized path),
`zone/client.cpp:~6684-6708` (`Client::SendGroupCreatePacket`, sent once to the group's founder),
and (implicitly) every member re-derives their own view. Confirmed layout:

```
Header:
  uint32   group_id_or_unused      // comment says "probably group ID", observed 0
  uint32   member_count            // total members in this packet, including the recipient
  cstr     leader_name             // NUL-terminated; EMPTY (single 0x00 byte) in the
                                    // "just formed, about to send OP_GroupLeaderChange separately" case

Then member_count records, index 0 is always the RECEIVING client's own entry (not necessarily
the leader):
  uint32   member_index            // 0 for self, 1..N for the rest (order = internal slot order)
  cstr     member_name             // NUL-terminated
  uint16   is_merc_flag            // 0 = not a merc
  cstr     merc_owner_name         // NUL-terminated; empty (single 0x00) when not a merc
  uint32   level
  uint8    group_tank_flag
  uint8    group_assist_flag
  uint8    group_puller_flag
  uint32   offline_flag
  uint32   timestamp
```
Per-member record size (non-merc) = `22 + strlen(name) + 1` bytes — this exact formula is used by
the server to precompute `PacketLength` before allocating the packet (`rof2.cpp:1625-1635`,
mirrored in `zone/client.cpp` `SendGroupCreatePacket`). Header size = `8 + strlen(leader_name) + 1`.

`Group::SendUpdate()` (`zone/groups.cpp:1052-1080`) is the only caller that builds the
`groupActUpdate` (action=7) `GroupUpdate2_Struct` that triggers this translation — it is called
per-recipient (loop calls `member->CastToClient()->QueuePacket`), so **each group member gets their
own personalized `OP_GroupUpdateB` packet** (their own name always at index 0), not one shared
broadcast buffer.

### GroupLeaderChange_Struct — 148 bytes, common struct, NOT overridden for RoF2
`common/eq_packet_structs.h:2544-2549`:
```c
struct GroupLeaderChange_Struct {
/*000*/ char Unknown000[64];
/*064*/ char LeaderName[64];
/*128*/ char Unknown128[20];
};
```
No `ENCODE`/`DECODE` handler for `OP_GroupLeaderChange` exists in `rof2.cpp` — confirmed by grep
(only 3 hits for "GroupLeaderChange" repo-wide, all in `zone/*.cpp`, none in the patch file). It
passes straight through with this common 148-byte layout. Only `LeaderName` is populated
(`Client::SendGroupLeaderChangePacket`, `zone/client.cpp:~6710-6718`); `Unknown000`/`Unknown128`
are left as whatever `new EQApplicationPacket(...)` zero-initializes them to.

### GroupMakeLeader_Struct (client→server, `/makeleader`) — 456 bytes
`common/eq_packet_structs.h:5860-5867`: `uint32 Unknown000; char CurrentLeader[64]; char
NewLeader[64]; char Unknown072[324];`. Only `NewLeader` is read server-side
(`Client::Handle_OP_GroupMakeLeader`, `zone/client_packet.cpp:7519-7538`).

### GroupRole_Struct (main tank / main assist / puller toggle) — 148 bytes
`common/eq_packet_structs.h:5617-5628`: `Name1[64]`, `Name2[64]`, three `uint32` unknowns,
`RoleNumber` (1=tank, 2=assist, 3=puller), `Toggle` (uint8), 3 pad bytes.

## 3. End-to-end invite flow

All group actions are **server-authoritative** — the client sends an intent packet (invite,
follow/accept, disband/leave/kick), and the server validates, mutates the authoritative `Group`
object (`zone/groups.cpp`), and then re-broadcasts state to every affected client via fresh
`OP_GroupUpdate`(→translated)/`OP_GroupDisbandYou`/`OP_GroupDisbandOther`/`OP_GroupLeaderChange`
packets. The client does not locally apply the roster change before server confirmation; there's
no "optimistic" invite UI implied by the wire protocol.

**(a) A invites B, no existing group (open invite):**
1. A targets B (or names B, depending on `Character:GroupInvitesRequireTarget` rule) and sends
   `OP_GroupInvite`/`OP_GroupInvite2` (`invitee_name=B, inviter_name=A`).
2. `Client::Handle_OP_GroupInvite2` (`zone/client_packet.cpp:7431-7517`) looks up B. If B is
   ungrouped/merc-only and same-zone, the packet (or a repackaged fixed `OP_GroupInvite`, if it
   arrived as `OP_GroupInvite2`) is forwarded directly to B's client
   (`invitee->CastToClient()->QueuePacket(...)`, line 7469). If B is in a different zone, a
   `ServerOP_GroupInvite` message goes to `world` for routing (line 7510-7513).
3. B's client presents the invite popup (client-rendered only — not itself a distinct opcode; the
   client interprets the incoming `OP_GroupInvite` packet as "you were invited").

**(b) A invites B while A already has a group:**
Same as (a) — `Handle_OP_GroupInvite2` doesn't special-case "already grouped"; the invite is still
just forwarded to B. Group creation/merge logic lives entirely in step (c) below
(`Group::AddMember`, `zone/groups.cpp:223-...`), which finds A's existing group via
`entity_list.GetGroupByClient(inviter)` (`zone/client.cpp:5127`) and adds B to it rather than
creating a new one (`Client::GroupFollow`, `zone/client.cpp:5052-5175`).

**(c) B accepts:**
1. B's client sends `OP_GroupFollow`/`OP_GroupFollow2` (`name1=A, name2=B`).
2. `Client::Handle_OP_GroupFollow2` (`zone/client_packet.cpp:7382-7423`) looks up A. If same-zone,
   calls `this->GroupFollow(A)`.
3. `Client::GroupFollow` (`zone/client.cpp:5052-...`):
   - If A has no group yet: creates one (`new Group(inviter)`), sets A as leader
     (`database.SetGroupLeaderName`), and for SoD+/RoF2 clients sends A three packets in order:
     `SendGroupCreatePacket()` (streamed `OP_GroupUpdateB`, member_count=1, leader_name=empty),
     `SendGroupLeaderChangePacket(A)` (`OP_GroupLeaderChange`), `SendGroupJoinAcknowledge()`
     (`OP_GroupAcknowledge`, 4 bytes — triggers "You have joined the group").
   - Then calls `Group::AddMember(B, ...)` (`zone/groups.cpp:223-...`), which sends every existing
     member (including A) a `groupActJoin` `OP_GroupUpdate`→`GroupJoin_Struct` packet with B's name,
     and updates each client's `PlayerProfile.groupMembers[]` array server-side
     (`GetPP().groupMembers[...]`, lines 296-319) — this is the same PP array eqoxide already knows
     is delivered via the streamed `OP_PlayerProfile` (see `eq-rof2-playerprofile-streamed.md`
     equivalent finding in the top-level memory notes), so initial roster on zone-in/char-select
     can also come from the profile blob, not just live group opcodes.
4. Reply: if A and B are same-zone, `GroupFollow` (via `Handle_OP_GroupFollow2`) also echoes an
   `OP_GroupFollow` back to A notifying the invite was accepted (line 7405-7407).

**(d) B declines:**
1. B's client sends `OP_GroupCancelInvite` (`GroupCancel_Struct{name1=?, name2=?, toggle}` — decline
   button presumably sets `toggle`, but server doesn't branch on it).
2. `Client::Handle_OP_GroupCancelInvite` forwards the packet verbatim to A (same-zone) or via
   `ServerOP_GroupCancelInvite`/world (cross-zone), then calls `Group::RemoveFromGroup(this)`
   unconditionally as a safety no-op (B was never actually added to a group yet in the normal
   decline case, so this is mostly a defensive cleanup call).
3. A's client receives the same `OP_GroupCancelInvite` packet back and is expected to render the
   decline message client-side (not confirmed — EQ typically resolves these via server
   `MessageString` IDs sent separately, not this packet's payload).

**(e) Leader (or an authorized member) kicks a member:**
`Client::Handle_OP_GroupDisband` (`zone/client_packet.cpp:7195-7375`) is overloaded for both kick
and voluntary leave — same opcode, same struct, disambiguated by whether the sender is the group
leader and who the target is:
- If sender is leader and `GetTarget()` (or `gd->name2`) names another member → the leader can kick
  that member: `group->DelMember(memberToDisband, false)` (line 7339).
- If sender is leader and targets themself (or no target while `group->GroupCount() < 3`) → the
  whole group is disbanded (`group->DisbandGroup()`, various branches lines 7302-7329).
- If sender is a non-leader member → they can only remove themselves (`group->DelMember(this,
  false)`, line 7355) — the code explicitly disallows a regular member kicking someone else.
`Group::DelMember` (`zone/groups.cpp:649-...`) sends the departing/kicked member (and everyone else)
a `groupActLeave` `OP_GroupUpdate` → this gets rewritten by `rof2.cpp:1576-1611` into
`OP_GroupDisbandYou` (sent to the person who left/was kicked) and `OP_GroupDisbandOther` (sent to
every remaining member, describing who left).

**(f) A member voluntarily leaves (not the leader), or disbands as leader with <3 members:**
Same `OP_GroupDisband` path as (e) — see branches above. If the group drops below 3 total members
(`group->GroupCount() < 3`), the server just disbands the whole group outright rather than
reshuffling (`zone/client_packet.cpp:7302-7307`), and `Group::DisbandGroup()`
(`zone/groups.cpp:915-988`) sends every remaining client a `groupActDisband` `OP_GroupUpdate` →
also rewritten to `OP_GroupDisbandYou`/`OP_GroupDisbandOther` by the same RoF2 encode branch.

**(g) Leader reassignment when the leader leaves — CONFIRMED QUIRK / DEAD CODE:**
`Group::DelMember` (`zone/groups.cpp:649-664`) has this at the very top:
```c
if (oldmember == GetLeader()) {
    DisbandGroup();
    return true;
}
```
This means **when the leader leaves via `DelMember`, the entire group is unconditionally
disbanded** — there is no leader handoff in this path. Further down in the same function
(lines 686-705) there is leader-reassignment logic (`ChangeLeader(members[nl])` picking the first
remaining client) guarded by `if (oldmember == GetLeader() && GroupCount() >= 2)` — but this is
**dead code**, unreachable because the early `return true` at line 663 already exits the function
whenever `oldmember == GetLeader()`. The source itself flags this with a comment: *"TODO: fix this
shit... So instead of figuring it out now, lets just disband the group..."*
(`zone/groups.cpp:656-660`). **Confirmed via direct code read, not inferred** — this is current
EQEmu behavior a RoF2-interoperable client must match: leaving-as-leader always fully disbands the
group; the only way to hand off leadership without disbanding is the explicit `/makeleader`
command (`OP_GroupMakeLeader` or the `groupActMakeLeader` `OP_GroupUpdate` variant), which calls
`Group::ChangeLeader()` (`zone/groups.cpp:2340-2370`) directly and does NOT disband anything — it
sends a `groupActMakeLeader` `OP_GroupUpdate`→`GroupJoin_Struct` to all members plus (for SoD+/RoF2
clients) a dedicated `OP_GroupLeaderChange` packet (line 2363-2364).

## 4. Group roster window / HP tracking

See `hp-update.md` for the full correction. Summary: group membership does **not** unlock full
cur/max HP for other members — it unlocks the **percent** triad
`OP_MobHealth`/`OP_MobManaUpdate`/`OP_MobEnduranceUpdate` (each 3 bytes, keyed by `spawn_id`) via
`Group::SendHPPacketsFrom()` (`zone/groups.cpp:428-450`), called from `Mob::SendHPUpdate()`
whenever `IsGrouped()` (`zone/mob.cpp:1590-1596`). Only the player's own HP arrives as the full
10-byte `OP_HPUpdate` (`SpawnHPUpdate_Struct: spawn_id, cur_hp, max_hp`), and only on that player's
own connection (`zone/mob.cpp:1522-1549`).

The persistent roster (names, count, leader flag) itself comes from:
- `OP_GroupUpdateB` (streamed, full roster snapshot — see §2) whenever `Group::SendUpdate()` fires
  (member add/AA change/etc.), and
- the `PlayerProfile.groupMembers[]` array baked into the (also-streamed) `OP_PlayerProfile` on
  zone-in — populated in `Group::AddMember` (`zone/groups.cpp:296-319`).

Recommendation: track roster membership from `OP_GroupUpdateB` + `PlayerProfile.groupMembers[]`,
and drive each member's live HP/mana/endurance bar from `OP_MobHealth`/`OP_MobManaUpdate`/
`OP_MobEnduranceUpdate` keyed by `spawn_id` (which requires the roster→spawn_id mapping to be
resolved via the normal spawn list, since none of these group packets carry a name↔spawn_id
binding themselves beyond the initial roster snapshot's member names).

## 5. Known EQEmu RoF2 group-code quirks worth flagging

1. **Leader-leaves-always-disbands** (see §3g) — `zone/groups.cpp:649-664` vs the dead
   reassignment code at `zone/groups.cpp:686-705`. A from-scratch client should not expect (or try
   to render) an automatic leader handoff when the leader simply leaves/quits; it will see a full
   disband instead.
2. **`OP_GroupUpdate` is a single app-opcode fanning out to 4+ different real wire opcodes**
   (`OP_GroupUpdateB`, `OP_GroupDisbandYou`, `OP_GroupDisbandOther`, or a raw `GroupJoin_Struct`
   still under the `OP_GroupUpdate` wire opcode) depending on `action`/`in->size`
   (`common/patches/rof2.cpp:1569-1717`). eqoxide must dispatch on the **wire opcode actually
   received** (`OP_GroupUpdateB` vs `OP_GroupUpdate` vs `OP_GroupDisbandYou`/`Other`), not assume
   `OP_GroupUpdate` always carries one fixed struct.
3. **`OP_GroupUpdateB` is streamed/variable-length**, same category of bug-risk as
   `OP_PlayerProfile` — a fixed-`repr(C)` struct will not parse it; needs a byte-cursor
   reader honoring the `VARSTRUCT_ENCODE_STRING`/NUL-terminated-string + fixed-field interleaving
   documented in §2.
4. **`OP_GroupDisband` size — RESOLVED, was the single highest-risk struct-size gotcha in this
   subsystem, directly analogous to the RoF2 door 80-vs-100-byte bug (`eq-rof2-door-struct`).**
   The static-analysis inference (no `ENCODE`/`DECODE` handler for `OP_GroupDisband` in
   `rof2.cpp` -> must be the common 128-byte `GroupGeneric_Struct`) was **confirmed wrong by a
   live packet capture** during task-6 validation (2026-07-01): the running EQEmu zone server
   rejected a 128-byte `OP_GroupDisband` payload with `Wrong size on incoming [OP_GroupDisband]
   (structs::GroupGeneric_Struct): Got [128], expected [148]`, logged it as `OP_Unknown`, and
   silently dropped it (no crash, no error to client, no roster change on either side). The
   client must send the **148-byte RoF2-namespaced** `GroupGeneric_Struct` (`name1[64]`,
   `name2[64]`, 5 trailing zero uint32s) for this opcode too -- same shape as `OP_GroupInvite`/
   `OP_GroupFollow`. Fixed in `build_group_disband()` (`src/eq_net/navigation.rs`).
5. **`OP_GroupCancelInvite`'s RoF2 hex value is listed as `0x0000`** in `patch_RoF2.conf:539` —
   unlike other `0x0000` entries in that file which usually mean "not remapped for this client",
   this one has full `ENCODE`/`DECODE` handlers implemented in `rof2.cpp`, so the `0x0000` is
   suspicious/possibly a placeholder bug in the conf itself. Confirm the real numeric opcode from a
   live RoF2 packet capture rather than trusting the conf value verbatim.
6. **Bot/Merc/Raid branches interleave with plain group code** throughout `zone/groups.cpp` and
   `Handle_OP_GroupDisband`/`Handle_OP_GroupInvite2` — if eqoxide's own character is ever grouped
   with a merc or in a raid, expect the wire traffic to diverge from the plain-group paths
   described above (raid uses entirely different `Raid::*` broadcast packets, not covered in this
   report).

## Open gaps / not yet investigated here

- `OP_SetGroupTarget` (`0x2814`) and `OP_AssistGroup` (`0x27f8`) — opcodes exist in the RoF2 table
  but their structs/handlers weren't traced for this report.
- `OP_DoGroupLeadershipAbility` / `OP_GroupLeadershipAAUpdate` full field semantics (Mark NPC,
  off-tank delegation) — struct location identified (`common/eq_packet_structs.h:2526-2534,
  4836-4857`) but not traced end-to-end.
- Client-side rendering/popup logic for invite/decline (what exact UI element pops up, whether
  there's a timeout) is not recoverable from the server source — cheapest next step would be a
  live packet capture + client screen recording.
