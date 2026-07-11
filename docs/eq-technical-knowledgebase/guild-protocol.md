# Guild protocol (RoF2)

Status: all findings below are **confirmed** by direct code read against
`/home/dhenry/git/EQEmu` (RoF2 patch files + zone server logic), current as of this repo's
HEAD. Nothing here is from the decompiled client — `eqgame.exe.c` has no guild-related literal
strings (server `MessageString` IDs drive UI text), so client-side rendering/UI behavior is
inferred from server packet-construction comments, not disassembled. Cross-reference
`group-protocol.md` — guild follows the same "server-authoritative, client sends intent, server
re-broadcasts state" shape, with the added wrinkle that `OP_GuildMemberList` uses **network byte
order** (a real outlier in this codebase) and `OP_PlayerProfile`'s guild fields sit **after a
variable-length region**, making a fixed offset there unsafe (use the spawn stream instead — see
§1).

## 0. Sentinels and rank scale

- **`GUILD_NONE = 0xFFFFFFFF`** (`common/guilds.h:22`) — "no guild" sentinel for `guild_id`.
  `IsInAGuild()` (`zone/client.h:843`) treats **both** `guild_id == 0` and
  `guild_id == GUILD_NONE` as "not in a guild" (`guild_id != GUILD_NONE && guild_id != 0`) — so
  eqoxide should treat `guild_id == 0 || guild_id == 0xFFFFFFFF` as "no guild", not just one of
  them.
- **Rank scale (RoF2, `common/guilds.h:39-47`) — NOT 0=member/1=officer/2=leader.** The real RoF2
  scale (used everywhere in `zone/guild.cpp`/`guild_mgr.cpp` and on the wire in
  `GuildMemberEntry_Struct.rank`/spawn+profile `guildrank`) is:
  ```
  0 = GUILD_RANK_NONE
  1 = GUILD_LEADER
  2 = GUILD_SENIOR_OFFICER
  3 = GUILD_OFFICER
  4 = GUILD_SENIOR_MEMBER
  5 = GUILD_MEMBER
  6 = GUILD_JUNIOR_MEMBER
  7 = GUILD_INITIATE
  8 = GUILD_RECRUIT      (GUILD_MAX_RANK = 8)
  ```
  A stray comment in `rof2_structs.h:2080` (`GuildJoin_Struct.rank`, "0 member, 1 officer, 2
  leader") is **stale Titanium-era text** (`GUILD_MEMBER_TI=0/GUILD_OFFICER_TI=1/GUILD_LEADER_TI=2`,
  `common/guilds.h:33-36`) left over from a struct that's shared code but not actually used this
  way for RoF2↔RoF2 traffic — only the cross-version translation paths in
  `zone/client_packet.cpp` (`Handle_OP_GuildInvite`/`Handle_OP_GuildInviteAccept`, guarded by
  `ClientVersion() < RoF`) touch the old 0/1/2 scale. **eqoxide (a RoF2 client) always uses the
  0-8 scale above.** Lower number = higher rank; rank 1 (leader) is the top.
- `guild_id < RoF2::constants::MAX_GUILD_ID` (`= 50000`, `common/patches/rof2_limits.h:305`) is
  just a sanity filter used when building the id→name table (§2) to skip bogus/sentinel ids —
  not a real guild-count cap.

## 1. Guild identity of the player (guild_id / guild_rank)

### `OP_ZoneSpawns`/`OP_NewSpawn` (Spawn_Struct stream) — RECOMMENDED SOURCE

Confirmed in `common/patches/rof2.cpp:4805-4820` (the streamed spawn-appearance encoder eqoxide
already parses in `parse_rof2_spawn`, `src/eq_net/protocol.rs:478`):
```c
if (emu->NPC) {
    VARSTRUCT_ENCODE_TYPE(uint32, Buffer, 0xFFFFFFFF);   // guildID   (NPCs: always "no guild")
    VARSTRUCT_ENCODE_TYPE(uint32, Buffer, 0x00000000);   // guildrank
} else {
    VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->guildID);
    VARSTRUCT_ENCODE_TYPE(uint32, Buffer, emu->guildrank);   // RoF2 0-8 scale, see §0
}
```
This sits right after `holding(u8)` and `deity(u32)` in the stream, before `class_(u8)`. eqoxide's
`parse_rof2_spawn` **already walks past this exact position** but currently discards it:
`src/eq_net/protocol.rs:586-589` — `skip!(1)` (holding) then `skip!(12)` (deity + guildID +
guildrank, 3×u32). Change the `skip!(12)` to `rd_u32!()` deity (discard) + `rd_u32!()` guildID
(keep) + `rd_u32!()` guildrank (keep). Because `parse_rof2_spawn` is a proper sequential
byte-cursor parser (not a fixed-offset struct read), this is safe and trivial — and it's the
**same code path already used for every spawn**, including the player's own self-spawn record
(`register_spawn` already special-cases `info.name == gs.player_name` to populate
`gs.player_*`, `src/game_state.rs`/`packet_handler.rs:1903-1929`). No new opcode plumbing needed —
just widen `SpawnInfo` with `guild_id: u32, guild_rank: u32` and capture them in that same
self-branch.

NPC sentinel: `guildID = 0xFFFFFFFF`, `guildrank = 0` for every NPC — matches `GUILD_NONE`.

### `OP_PlayerProfile` — guild fields exist but are AT A VARIABLE OFFSET, avoid a fixed-offset read

Confirmed in `common/patches/rof2.cpp:2964-2966` (inside `ENCODE(OP_PlayerProfile)`):
```c
outapp->WriteUInt8(0);              // Unknown
outapp->WriteUInt8(emu->pvp);
outapp->WriteUInt8(0);              // Unknown
outapp->WriteUInt8(emu->gm);
outapp->WriteUInt32(emu->guild_id);
outapp->WriteUInt8(emu->guildrank); // guildrank — note: ONE BYTE here, not u32 (unlike the spawn stream!)
outapp->WriteUInt32(0);             // Unknown
outapp->WriteUInt8(0);              // Unknown
outapp->WriteUInt32(0);             // Unknown
outapp->WriteUInt64(emu->exp);      // int32 in client
```
immediately after the fixed `name[64]`/`last_name[32]`/birthday/lastlogin/timePlayedMin/
timeentitledonaccount/expansions/languages/`zone_id(u16)`/`zoneInstance(u16)`/`y,x,z,heading(4×f32)`
block. **This is genuinely reachable by walking the stream sequentially from a known anchor**, but
eqoxide's existing `parse_player_profile` (`src/eq_net/packet_handler.rs:1020`) is a
**fixed-byte-offset** reader, and guild_id/guildrank sit **after two `WriteString()` calls for
bandolier names/item names and potion-belt item names**
(`common/patches/rof2.cpp:2839-2885`, `WriteString` = NUL-terminated, variable length, no
padding — `common/base_packet.h:69`). Those are earlier in the stream than guild_id. A character
with any saved bandolier/potion-belt loadout shifts every fixed-offset field after that point by a
variable, character-dependent amount — so **a hardcoded byte offset for guild_id/guildrank in
`OP_PlayerProfile` will silently read garbage for any character who has ever saved a bandolier or
potion belt entry.** (eqoxide's currently-trusted offsets — `class_@21`, `level@22`, `mana@944`,
`cur_hp@948`, `STR@952`..`WIS@976`, `mem_spells@9784`, `coin@13269` — are all **safe** because
they sit *before* the bandolier/potion-belt write at line 2839; don't extend that fixed-offset
trick past it.)

**Recommendation: do NOT add a fixed offset for guild_id/guildrank in `parse_player_profile`.**
Rely on the spawn-stream path (above) instead — it arrives moments after `OP_PlayerProfile` during
zone-in via `OP_ZoneSpawns`, is a real sequential parse, and needs zero extra offset archaeology.
If a future need requires reading it out of `OP_PlayerProfile` specifically, do a proper
sequential/streamed parse from the `zone_id` anchor forward (skip zoneInstance, y/x/z/heading,
2×u8 pad+pvp, u8 pad+gm, then read guild_id u32 + guildrank **u8**), not a hardcoded byte index.

### Sentinel

`guild_id == 0` or `guild_id == 0xFFFFFFFF` ⇒ no guild (§0). `guildrank` is meaningless when not
guilded (server sends `GUILD_RANK_NONE = 0` after `RefreshGuildInfo()` clears it,
`zone/guild.cpp:245-246`).

## 2. `OP_GuildsList` (0x507a) — server-wide guild id→name directory

Confirmed `ENCODE(OP_GuildsList)` (`common/patches/rof2.cpp:1873-1905`) and
`Client::SendGuildList()` (`zone/guild.cpp:192-206`, builds `guild_mgr.MakeGuildList()` →
`BaseGuildManager::MakeGuildList()`, `common/guild_base.cpp:946-964`).

**This is a full server-wide table of every guild that exists on the server (not just the
player's own guild)** — used by the client to resolve `guild_id → name` for the guild tag shown
over any player's head (their spawn record only carries the numeric `guild_id`).

Wire format (all **little-endian**, `VARSTRUCT_ENCODE_TYPE`/native writes — `common/misc_functions.h:33`):
```
u8[64]   header            // always zero-filled (memset(buf_pos, 0, 64) in the encoder)
u32      no_of_guilds       // total guild count server-wide
repeat (no_of_guilds times, but only guilds with guild_id < MAX_GUILD_ID=50000 are actually emitted):
    u32      guild_id
    cstr     guild_name     // NUL-terminated, variable length
```
Not a fixed-width/fixed-index array — parse it as a byte cursor: skip 64, read `count`, then loop
`count` times reading `(u32, cstr)` pairs (loop may emit fewer than `no_of_guilds` entries if any
guild ids are ≥ `MAX_GUILD_ID`, which in practice shouldn't happen for normal DB-assigned ids).
Build an `id → name` `HashMap<u32, String>` from this and cache it — it's the only way to turn the
`guild_id` from §1 into a display name.

**When it's sent:** once per zone-in, for every client regardless of guild membership
(`zone/client_packet.cpp:892`, inside `Client::CompleteConnect()`), and again whenever any guild
is created/renamed/deleted/a new guild appears (`zone/guild_mgr.cpp:317,409,605` /
`zone/guild.cpp:702,761` all call `SendGuildList()`/`entity_list.SendGuildList()`) — so eqoxide
should treat a re-received `OP_GuildsList` as "merge/replace the whole id→name map", not "ignore
after first receipt".

## 3. Roster: `OP_GuildMemberList` (full snapshot) + incremental update opcodes

### `OP_GuildMemberList` (0x12a6) — full roster snapshot, **NETWORK BYTE ORDER (big-endian) — the one outlier in this protocol**

Confirmed `ENCODE(OP_GuildMemberList)` (`common/patches/rof2.cpp:1764-1856`) — every numeric field
in the per-member records is written with `htonl`/`htons`. `GuildMemberEntry_Struct`
(`common/patches/rof2_structs.h:3685-3702`) even has the struct comment *"Other than the strings,
all of this packet is network byte order (reverse from normal)"*. **Everything else in the RoF2
protocol eqoxide has documented so far is little-endian** — this is a deliberate, easy-to-miss
trap. Byte-swap every numeric field when parsing this one opcode.

Wire layout (variable length):
```
cstr     prefix_name        // caller-supplied "prefix" string (own name or guild name depending
                              // on call site — SendGuildMembers() passes GetName(), the more
                              // common SendGuildMembersList() passes the guild's own name);
                              // not meaningful roster data, just skip it.
u32BE    <unset/garbage>     // "guild ID" field — CONFIRMED the encoder skips writing this
                              // (`buffer += sizeof(uint32)` with the actual write commented out,
                              // rof2.cpp:1786-1788) — bytes are whatever `new uint8[length]` left
                              // uninitialized. DO NOT rely on this field for the guild id; you
                              // already know your own guild_id from §1.
u32BE    member_count
repeat member_count times (GuildMemberEntry_Struct, "52 bytes + strings"):
    cstr     name                // member char name, NUL-terminated
    u32BE    level
    u32BE    banker              // bit0 = banker, bit1 = alt  (0/1/2/3)
    u32BE    class_
    u32BE    rank                 // RoF2 0-8 scale, §0
    u32BE    time_last_on         // unix timestamp, last login (updated live if online — see below)
    u32BE    tribute_enable
    u32BE    unknown01            // always 0
    u32BE    total_tribute
    u32BE    last_tribute         // unix timestamp
    u32BE    unknown_one          // always 1
    cstr     public_note          // NUL-terminated (may be empty)
    u16BE    zoneinstance         // always 0 in RoF2 (struct comment: "seen 0s or -1")
    u16BE    zone_id              // **THE ONLINE FLAG**: 0 = OFFLINE, nonzero = ONLINE (and this
                                    // is the zone they're currently in) — see MakeGuildMembers,
                                    // zone/guild_mgr.cpp:1521-1526: "if (ci->online) zone_id =
                                    // ci->zone_id; else zone_id = 0"
    u32BE    unknown_one2         // always 1
    u32BE    unknown04            // always 0
```
There is **no separate online boolean** — `zone_id != 0` **is** the online flag, and doubles as
"what zone are they in". This is the field to key eqoxide's `online`/`zone` roster columns off.

**When it's sent:** `Client::SendGuildMembersList()` (`zone/guild.cpp:522-545`) — called on every
zone-in for a guilded client (`zone/client_packet.cpp:887,894`, inside `CompleteConnect()`), and
any time the roster is "dirty" (`SetGuildListDirty(true)`, e.g. after a member add/remove was
missed/batched). It also triggers `SendGuildMOTD()`/`SendGuildChannel()`/`SendGuildURL()`
immediately afterward as a bundle — expect those 3 extra opcodes right after
`OP_GuildMemberList`.

### Incremental per-member update opcodes — fixed-size, LITTLE-ENDIAN structs (no RoF2 ENCODE override, so the common `eq_packet_structs.h` layout applies verbatim)

These are the opcodes that keep an already-connected client's roster in sync without a full
resend — **this is the primary "reflect a server-pushed change" path for D.6** (a GM `#guild add`
issues exactly these):

| Opcode | Hex | Struct (`common/eq_packet_structs.h`) | Size | Sent when |
|---|---|---|---|---|
| `OP_GuildMemberAdd` | `0x2925` | `GuildMemberAdd_Struct` (`:1762-1774`): `guild_id(u32) unknown04(u32) unknown08(u32) unknown12(u32) level(u32) class_(u32) rank_(u32) guild_show(u32) zone_id(u32) last_on(u32) player_name[64]` | 104B | New member joins (`EntityList::SendGuildMemberAdd`, `zone/guild.cpp:684-721`) — sent to **every** online guild member including the new member themself |
| `OP_GuildMemberDelete` | `0x3141` | `GuildMemberDelete_Struct` (`:1819-1822`): `guild_id(u32) player_name[64]` | 68B | Member removed/leaves (`EntityList::SendGuildMemberRemove`, `zone/guild.cpp:740-767`) |
| `OP_GuildMemberLevel` | `0x1bd3` | `GuildMemberLevel_Struct` (`:1776-1780`): `guild_id(u32) player_name[64] level(u32)` | 72B | A member levels up |
| `OP_GuildMemberRankAltBanker` | `0x0b9c` | `GuildMemberRank_Struct` (`:1782-1788`): `guild_id(u32) rank_(u32) player_name[64] alt_banker(u32) offline(u32)` | 80B | Rank/alt/banker flag changed. `alt_banker`: bit0=banker, bit1=alt |
| `OP_GuildMemberPublicNote` | `0x01f9` | `GuildMemberPublicNote_Struct` (`:1790-1794`): `guild_id(u32) player_name[64] public_note[256]` | 324B | Public note edited |
| `OP_GuildMemberRename` | `0x3b26` | `GuildRenameMember_Struct` (`:1805-1809`): `guild_id(u32) player_name[64] new_player_name[64]` | 132B | Member renamed (rare) |
| `OP_GuildMemberDetails` | `0x69b9` **(same hex as `OP_GuildMemberUpdate`!)** | `GuildMemberDetails_Struct` (`:1811-1817`): `guild_id(u32) player_name[64] zone_id(u32) last_on(u32) offline_mode(u32)` | 80B | `Client::SendGuildMemberDetails` — not observed called from any current EQEmu call site in this checkout (`zone/guild.cpp:647-660` defines it but nothing calls it) — likely dead/legacy. **Because it shares 0x69b9 with `OP_GuildMemberUpdate`, dispatch on struct SIZE (80 bytes for both, coincidentally — see next row) or just don't special-case it; `OP_GuildMemberUpdate`'s layout below is the one actually sent.** |
| `OP_GuildMemberUpdate` | `0x69b9` | `GuildMemberUpdate_Struct` (`common/patches/rof2_structs.h:3735-3743`): `GuildID(u32) MemberName[64] ZoneID(u16) InstanceID(u16) LastSeen(u32) Unknown76(u32)` | 80B | **Login/logout/zone-change live presence ping** — `ZoneGuildManager::SendGuildMemberUpdateToWorld()` (`zone/guild_mgr.cpp:655`) called with `ZoneID=0` on disconnect/camp (`zone/client_process.cpp:173,202,594`, `zone/client.cpp:721`) and `ZoneID=zone->GetZoneID()` on zone-in (`zone/client_packet.cpp:890`). **`ZoneID == 0` ⇒ that member just went offline; nonzero ⇒ online in that zone.** Confirmed **LITTLE-ENDIAN** — `ENCODE(OP_GuildMemberUpdate)` (`rof2.cpp:1857-1871`) uses plain `OUT()`/direct-copy macros, no `htonl` (unlike §3's `OP_GuildMemberList`) |
| `OP_GuildRenameGuild` | `0x61db` | `GuildRenameGuild_Struct` (`:1800-1803`): `guild_id(u32) new_guild_name[64]` | 68B | Guild itself renamed — also triggers a fresh `OP_GuildsList` (§2) |

**Recommendation:** track roster as `Vec<GuildMember>` (mirroring `GroupMember` in
`src/game_state.rs:240`) keyed by name; `OP_GuildMemberList` = full replace (mirror
`apply_group_update_b`'s pattern, `src/eq_net/packet_handler.rs:296-322`); `OP_GuildMemberAdd`
= upsert one; `OP_GuildMemberDelete` = remove one; `OP_GuildMemberUpdate` = patch
`online`/`zone_id`/`last_seen` on the matching name (this is the one that needs to update live
without a re-zone); `OP_GuildMemberLevel`/`RankAltBanker`/`PublicNote`/`Rename` = patch the
matching field. **Byte-swap only `OP_GuildMemberList`'s numeric fields; every other opcode in
this table is little-endian as usual.**

## 4. Guild identity live-update push: `OP_SpawnAppearance`, not a guild opcode

**This is the answer to D.6 for the player's OWN `guild_id`/`guild_rank`/`guild_show`.** eqoxide
already parses `OP_SpawnAppearance` (`OP_SPAWN_APPEARANCE = 0x0971`, `src/eq_net/protocol.rs:97`,
handled by `apply_spawn_appearance`, `src/eq_net/packet_handler.rs:1682-1692`, wire struct
confirmed identical to eqoxide's existing 8-byte parse: `spawn_id(u16) type(u16) parameter(u32)`,
`common/patches/rof2_structs.h:748-753`). Guild membership changes push **three** of these to the
affected client's own spawn id, using `AppearanceType` values from `common/eq_constants.h:38-64`:

| `type` (AppearanceType) | value | `parameter` meaning |
|---|---|---|
| `GuildID` | `22` | new `guild_id` (or `GUILD_NONE=0xFFFFFFFF` when removed) |
| `GuildRank` | `23` | new `guildrank` (RoF2 0-8 scale, §0) — the value sent is `guild_mgr.GetDisplayedRank(...)`, i.e. already the final rank to show |
| `GuildShow` | `52` | 0/1 — whether the guild tag is displayed over the spawn (`GUILD_ACTION_DISPLAY_GUILD_NAME` permission) |

Confirmed call sites, all three sent together as a set on every guild-membership-affecting event:
- `Client::SendGuildSpawnAppearance()` (`zone/guild.cpp:176-190`) — the general-purpose "push my
  current guild state" helper, called after any local rank/permission change and at zone-in via
  `RefreshGuildInfo()` (`zone/guild.cpp:289`).
- `EntityList::SendGuildMemberAdd(...)` (`zone/guild.cpp:715-717`) — explicitly sends all 3 to the
  newly-added member right after their `OP_GuildMemberAdd`/full roster/`OP_GuildsList` bundle
  (§1/§2/§3).
- `EntityList::SendGuildMemberRemove(...)` (`zone/guild.cpp:762-763`) — sends `GuildID=GUILD_NONE`
  + `GuildRank=GUILD_RANK_NONE` (only 2 of the 3 — no `GuildShow` reset) to the removed member.

**A GM's `#guild add <char> <guild>` (`zone/gm_commands/guild.cpp:343`, calls
`guild_mgr.SetGuild(client, guild_id, GUILD_MEMBER)` → `ZoneGuildManager::SetGuild`,
`zone/guild_mgr.cpp:1729-1762`) drives exactly this same `EntityList::SendGuildMemberAdd` path** —
confirmed same code, so the full push sequence to the newly-added client (in order) is:
1. `OP_GuildMemberAdd` (to self + every other online guild member)
2. `OP_GuildsList` (full id→name directory — `SendGuildList()`)
3. `OP_GuildMemberList` (full roster snapshot — `SendGuildMembersList()`, itself followed by
   `OP_GuildMOTD`/`OP_GuildChannel`/`OP_GuildURL`)
4. `OP_GuildUpdate` (ranks/rank-names, RoF2+ only — `SendGuildRanks()`/`SendGuildRankNames()`,
   not documented in depth here; permission-matrix UI feed, safe to ignore for a headless client)
5. `OP_SpawnAppearance` × 3 (`GuildID`/`GuildRank`/`GuildShow`, self spawn id)

For a removal (`#guild remove`/leaving), the mirror is `OP_GuildMemberDelete` (to remaining
members) + for the removed client specifically: `SetGuildID(GUILD_NONE)`, `SendGuildList()`
(refreshed directory), `OP_SpawnAppearance`×2 (`GuildID=GUILD_NONE`, `GuildRank=GUILD_RANK_NONE`)
— **no fresh `OP_GuildMemberList` to the removed client** (they're no longer in any guild to
roster).

**Recommendation:** handle guild identity purely reactively — extend `apply_spawn_appearance`
with `kind == 22`/`23`/`52` cases gated on `id == gs.player_id`, updating
`gs.guild_id`/`gs.guild_rank`/`gs.guild_show` directly (same shape as the existing `ANIMATION`/
`SITTING` case at `packet_handler.rs:1687-1691`). Combined with §1 (spawn-stream `guildID`/
`guildrank` at zone-in) and §3 (roster opcodes), no guild-specific "am I in a guild now" polling
or extra request opcode is needed — the server pushes everything.

## 5. Join / leave / invite / remove — client→server opcodes

Real opcode names confirmed from `zone/client_packet.cpp:241-262` (`ConnectedOpcodes[...]`
table) — **the issue's guessed names `OP_GuildManageAdd`/`OP_GuildManageRemove` do not exist**;
the real opcodes are `OP_GuildInvite`/`OP_GuildInviteAccept`/`OP_GuildRemove`/`OP_GuildLeader`.

### `OP_GuildInvite` (`0x7099`) — invite AND promote/demote (same opcode, disambiguated server-side)

Struct: `GuildCommand_Struct`, RoF2-specific 140-byte layout
(`common/patches/rof2_structs.h:2096-2104`):
```c
struct GuildCommand_Struct {
/*000*/ char   othername[64];   // invite target / member being promoted-demoted
/*064*/ char   myname[64];      // sender's own name
/*128*/ uint16 guildeqid;       // sender's guild_id (server overwrites with GuildID() if 0)
/*130*/ uint8  unknown[2];      // comment: "for guildinvite all 0's, for remove 0=0x56,2=0x02"
/*132*/ uint32 officer;         // target RANK, RoF2 0-8 scale (§0) — NOT a boolean
/*136*/ uint32 unknown136;      // "New in RoF2"
};
```
`Client::Handle_OP_GuildInvite` (`zone/client_packet.cpp:8153-8331`): if `othername` is already in
the sender's guild, this is a promote/demote (`officer` compared against their current rank); if
`othername` is guildless, it's a fresh invite; if `othername` is in a *different* guild, it's
rejected with a chat message. **The server just forwards the packet verbatim to the target**
(`client->QueuePacket(app)`) — the invitee's client receives the exact same `OP_GuildInvite`/
`GuildCommand_Struct` back, and is expected to render an invite popup client-side (not verified —
no literal strings in the decompile). No response is required to dismiss/see it; only accepting
sends a new opcode (below).

For a plain invite (not promote), the real client is inferred (not directly disassembled) to send
`officer = GUILD_RECRUIT (8)` as the initial rank — this matches
`Handle_OP_GuildInvite`'s "not overridden for RoF2→RoF2" code path (only cross-version clients get
`officer` rewritten) and is the lowest/default rank in the 0-8 scale.

### `OP_GuildInviteAccept` (`0x7053`) — accept/decline

Struct: `GuildInviteAccept_Struct` (RoF2, `rof2_structs.h:2086-2091`, 136 bytes):
```c
struct GuildInviteAccept_Struct {
    char   inviter[64];
    char   newmember[64];   // the accepting player's own name
    uint32 response;        // rank to accept at (RoF2 0-8 scale) if accepting, or
                              // >= GUILD_INVITE_DECLINE(9) to decline (guilds.h:30)
    uint32 guildeqid;        // guild_id being joined
};
```
`Client::Handle_OP_GuildInviteAccept` (`zone/client_packet.cpp:8333-8414`): `response >=
GUILD_INVITE_DECLINE(9)` ⇒ declined (both parties get a chat message, no guild change).
`response < 9` ⇒ accepted at that rank: `c_invitee->SetGuildID(guild_id)`,
`guild_mgr.MemberAdd(...)` (which fans out `OP_GuildMemberAdd` + `OP_SpawnAppearance`×3 as in §4),
then `SendGuildSpawnAppearance()`. **Both inviter and invitee must be in the same zone** — if not,
the request is silently dropped with a "must be in the same zone" chat message (no cross-zone
guild-invite routing implemented for this path, unlike group invites which do route via world).

### `OP_GuildRemove` (`0x1444`) — leave (self) or leader/officer removes a member

**Same struct as invite** — `GuildCommand_Struct` (140 bytes, above). Confirmed
`DECODE(OP_GuildRemove)` (`rof2.cpp:5800-5811`) reads it as `GuildCommand_Struct` too.
`Client::Handle_OP_GuildRemove` (`zone/client_packet.cpp:8673-8724`): if
`othername == GetName()` (case-insensitive) ⇒ self-leave, always allowed while guilded; otherwise
requires `GUILD_ACTION_MEMBERS_REMOVE` permission at the sender's rank. Works for offline targets
too (looks up `CharGuildInfo` by name if not online in-zone). No separate "disband whole guild"
opcode is used by a member — a guild only ceases to exist when its last member leaves/is removed
(no explicit "OP_GuildDisband"-equivalent found for guilds, unlike the group protocol's
`OP_GroupDelete`).

### `OP_GuildLeader` (`0x7e09`) — transfer leadership

Struct: common (not RoF2-overridden) `GuildMakeLeader_Struct` (`common/eq_packet_structs.h:3979-3982`,
128 bytes): `char requestor[64]; char new_leader[64];`. `Client::Handle_OP_GuildLeader`
(`zone/client_packet.cpp:8416-8458`): sender must currently be `GUILD_LEADER` (rank 1); demotes
self to `GUILD_OFFICER` (3) and promotes `new_leader` to `GUILD_LEADER` (1). Target must be
same-zone.

### Not used for RoF2↔RoF2 traffic (found but out of scope for a minimal client)

`OP_GuildManageBanker`/`OP_GuildPromote`/`OP_GuildDemote`/`OP_GuildPublicNote`/`OP_GuildStatus`/
`OP_GuildCreate`/`OP_GuildDelete`/`OP_GuildWar`/`OP_GuildPeace`/tribute family — all real, all
`Handle_OP_Guild*`-dispatched, none needed for the "identity + roster + reflect + basic
invite/accept/remove" scope in issue #295.

## 6. eqoxide current state (this worktree, pre-#295)

Zero guild membership code exists — confirmed by full-repo grep:
- `OP_GUILD_LIST: u16 = 0x507a` is defined (`src/eq_net/protocol.rs:66`) but **unused** anywhere
  in `packet_handler.rs`.
- `src/eq_net/login.rs:495` lists `0x507a` in a `SILENT` drop-list during the zone-entry handshake
  phase — the packet is received and explicitly discarded, not "answered as empty" (the issue
  text's framing is slightly off — nothing is *sent back*; RoF2's `OP_GuildsList` is one-way
  S→C only, there's no client request/reply here to begin with, see §2).
- `parse_rof2_spawn` reads past `guildID`/`guildrank` with `skip!(12)` (protocol.rs:588-589) —
  parsed-position-correct but value-discarded (§1).
- `game_state.rs` has **no** `guild_id`/`guild_name`/`guild_rank`/`guild_members` fields at all
  (compare `player_id`/`group_members`/`group_leader` which do exist, `game_state.rs:272,461,463`).
- `apply_spawn_appearance` (`packet_handler.rs:1682-1692`) exists and already parses the exact
  8-byte struct needed for §4, but only handles `kind==14` (Animation/sitting) — no `GuildID`/
  `GuildRank`/`GuildShow` cases.
- No `http/guild.rs` module; `http/mod.rs` has no `guild`/`GuildShared`/`guild_invite`/etc.
  wiring (compare the `group`/`GroupShared`/`group_invite`/... wiring at
  `http/mod.rs:21,509-517,568-574` and the full `http/group.rs` module, which is the direct
  template to mirror per the issue's "mirroring the existing group implementation" ask).
- `navigation.rs` has `build_group_invite`/`build_group_follow`/`build_group_disband`/
  `build_group_make_leader` (148/152/148/456-byte builders, `navigation.rs:423-469`) as the
  pattern to mirror for `build_guild_invite`/`build_guild_invite_accept`/`build_guild_remove`
  (all three: 140/136/140-byte `GuildCommand_Struct`/`GuildInviteAccept_Struct`/
  `GuildCommand_Struct` per §5).

## Recommended eqoxide implementation shape (mirrors group exactly)

**Opcodes to add to `protocol.rs`** (values from `EQEmu/utils/patches/patch_RoF2.conf`, cited
inline above): `OP_GUILD_MEMBER_LIST=0x12a6`, `OP_GUILD_MEMBER_ADD=0x2925`,
`OP_GUILD_MEMBER_DELETE=0x3141`, `OP_GUILD_MEMBER_UPDATE=0x69b9`,
`OP_GUILD_MEMBER_LEVEL=0x1bd3`, `OP_GUILD_MEMBER_RANK_ALT_BANKER=0x0b9c`,
`OP_GUILD_MEMBER_PUBLIC_NOTE=0x01f9`, `OP_GUILD_INVITE=0x7099`,
`OP_GUILD_INVITE_ACCEPT=0x7053`, `OP_GUILD_REMOVE=0x1444`, `OP_GUILD_LEADER=0x7e09`
(`OP_GUILD_LIST` already exists).

**`GameState` additions** (`game_state.rs`, mirroring `GroupMember`/`group_members`/
`group_leader`): `guild_id: u32` (default `GUILD_NONE`/`0xFFFFFFFF`), `guild_name: String`
(resolved via the `OP_GuildsList` id→name cache), `guild_rank: u8`, `guild_show: bool`,
`guild_names: HashMap<u32, String>` (the §2 directory), `guild_members: Vec<GuildMember>` with
`{name, level, class_id, rank, zone_id, online, public_note, last_seen}` (`online = zone_id != 0`
per §3).

**Read/reflect path (priority per the issue):**
1. `parse_rof2_spawn`: stop discarding guildID/guildrank (§1) — cheapest, highest-value fix,
   touches code already being walked for every spawn including self.
2. `apply_spawn_appearance`: add `GuildID(22)`/`GuildRank(23)`/`GuildShow(52)` cases gated on
   `id == gs.player_id` (§4) — this is what makes a GM's `#guild add`/`#guild remove` reflect
   live without a re-zone.
3. New handler for `OP_GUILD_LIST` → populate `gs.guild_names` (§2, little-endian, cursor parse).
4. New handler for `OP_GUILD_MEMBER_LIST` → full-replace `gs.guild_members` (§3, **big-endian**
   numeric fields — the one opcode in this whole subsystem that needs byte-swapping).
5. New handlers for `OP_GUILD_MEMBER_ADD`/`DELETE`/`UPDATE`/`LEVEL`/`RANK_ALT_BANKER`/
   `PUBLIC_NOTE` → incremental upsert/patch on `gs.guild_members` by name (§3, little-endian).

**Write path (minimal per issue, lower priority):** `build_guild_invite(target, self_name,
guild_id)` → 140-byte `GuildCommand_Struct` with `officer=GUILD_RECRUIT(8)`;
`build_guild_invite_accept(inviter, self_name, guild_id, response)` → 136-byte
`GuildInviteAccept_Struct`; `build_guild_remove(self_name, target, guild_id)` → 140-byte
`GuildCommand_Struct` (same builder as invite, reused — matches `OP_GroupDisband` reusing
`GroupGeneric_Struct` for both leave and kick). `/v1/guild/{roster,invite,accept,leave,remove}`
mirroring `http/group.rs` 1:1 (leave = `build_guild_remove(self_name, self_name, ...)`, same
"self-target" trick `OP_GroupDisband` uses for group leave).
