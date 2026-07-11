# RoF2 chat: OP_ChannelMessage wire format and server-side routing

## Wire format (confirmed)

RoF2 `OP_ChannelMessage` is **not** the fixed Titanium `ChannelMessage_Struct`.
It is streamed/variable-length, NUL-terminated. Confirmed both directions in
the RoF2 patch:

- Server -> client ENCODE: `EQEmu/common/patches/rof2.cpp:1003-1034`
  (`ENCODE(OP_ChannelMessage)`):
  `sender\0 | targetname\0 | u32 unknown(0) | u32 language | u32 chan_num |
  u32 unknown(0) | u8 unknown(0) | u32 skill_in_language | message\0 |
  trailing u32 unknowns`.
- Client -> server DECODE: `EQEmu/common/patches/rof2.cpp:5338-5357+`
  (`DECODE(OP_ChannelMessage)`): decodes `sender`, `target`, skips 4 bytes,
  reads `u32 language`, `u32 chan_num`, skips 5 bytes, reads `u32 skill`,
  then the message. Same field order as ENCODE (this is a symmetric format).
- Opcode: `OP_ChannelMessage = 0x2b2d` — `EQEmu/utils/patches/patch_RoF2.conf:221`.

eqoxide's parser/builder (`src/eq_net/packet_handler.rs:1408-1422`,
`apply_channel_message`; `src/eq_net/navigation.rs:479-489`,
`build_channel_message`) already match this layout byte-for-byte.

## Channel enum (RoF2, confirmed)

`EQEmu/common/eq_constants.h:950-966` (`ChatChannel` enum):

```
ChatChannel_Guild      = 0
ChatChannel_Group      = 2
ChatChannel_Shout      = 3
ChatChannel_Auction    = 4
ChatChannel_OOC        = 5
ChatChannel_Broadcast  = 6
ChatChannel_Tell       = 7
ChatChannel_Say        = 8
ChatChannel_Petition   = 10
ChatChannel_GMSAY      = 11
ChatChannel_TellEcho   = 14
ChatChannel_Raid       = 15
ChatChannel_UCSRelay   = 20   (custom /join channels only — goes to UCS)
ChatChannel_Emotes     = 22
```

This matches the comment already in eqoxide's `packet_handler.rs:1429`
("0 guild, 2 group, 3 shout, 4 auction, 5 OOC, 6 broadcast, 7 tell, 8 say,
11 gmsay").

## Guild chat routing: zone/world, NOT UCS (confirmed, issue #294)

Guild chat (`/gu`) is sent and received as plain `OP_ChannelMessage` with
`chan_num = ChatChannel_Guild (0)` through the **zone** server, using the
exact same client-side struct/opcode as group (chan 2), auction, OOC, etc.
UCS is a completely separate service used only for `ChatChannel_UCSRelay`
(custom `/join <channel>` channels) — guild never touches it.

Full path, all confirmed in EQEmu:

1. **Client send**: client builds `OP_ChannelMessage` with `chan_num=0`,
   `targetname` empty (or whatever's currently targeted — unused for guild),
   `language`/`skill` as normal. Decoded by
   `EQEmu/common/patches/rof2.cpp:5338` into the internal
   `ChannelMessage_Struct`.
2. **Zone receive**: `Client::Handle_OP_ChannelMessage`
   (`EQEmu/zone/client_packet.cpp:4559-4586`) recomputes `language_skill`
   server-side from `m_pp.languages[cm->language]` (client's `skill` field on
   the wire is **ignored** on receipt — server is authoritative) and calls
   `ChannelMessageReceived(cm->chan_num, cm->language, language_skill,
   cm->message, cm->targetname)` at line 4584.
3. **`Client::ChannelMessageReceived`** (`EQEmu/zone/client.cpp:1201`),
   `case ChatChannel_Guild:` at **line 1349-1358**:
   ```cpp
   case ChatChannel_Guild: { /* Guild Chat */
       if (!IsInAGuild()) {
           MessageString(Chat::DefaultText, GUILD_NOT_MEMBER2);
       } else if (!guild_mgr.CheckPermission(GuildID(), GuildRank(),
                   GUILD_ACTION_GUILD_CHAT_SPEAK_IN)) {
           MessageString(Chat::EchoGuild, NO_PROPER_ACCESS);
       } else if (!worldserver.SendChannelMessage(this, targetname,
                   chan_num, GuildID(), language, lang_skill, message)) {
           Message(Chat::White, "Error: World server disconnected");
       }
       break;
   }
   ```
   This is handled **directly** in zone code — not delegated to UCS or any
   external process. The only guild-specific server-side gates are
   `IsInAGuild()` and the `GUILD_ACTION_GUILD_CHAT_SPEAK_IN` permission
   check; no extra wire fields are required from the client beyond the
   standard `OP_ChannelMessage` payload.
4. **Zone -> world**: `WorldServer::SendChannelMessage`
   (`EQEmu/zone/worldserver.cpp:4144-4189`) packages a
   `ServerPacket(ServerOP_ChannelMessage, ...)` (internal zone<->world
   protocol, `ServerChannelMessage_Struct`) with `guilddbid` set and sends it
   up to world. This is an **internal server-to-server** hop, not a new
   client-visible opcode.
5. **World routes by guild**: `World::ZoneServer::HandleMessage` (actually
   `EQEmu/world/zoneserver.cpp:423-576`), `case ServerOP_ChannelMessage`:
   guild/auction/OOC/broadcast/GMSAY messages are echoed to the console (GM
   tool) at line 545-566, then at **line 567-568**:
   ```cpp
   if (scm->guilddbid > 0) {
       ZSList::Instance()->SendPacketToZonesWithGuild(scm->guilddbid, pack);
   }
   ```
   i.e. world fans the message out to every zone process that currently has
   an online member of that guild (not a global broadcast, not UCS).
6. **Target zone -> clients**: each such zone's
   `EntityList::ChannelMessageFromWorld`
   (`EQEmu/zone/entity.cpp:2275-2295`) iterates its local `client_list`,
   filters by `chan_num == ChatChannel_Guild -> client->IsInGuild(guild_id)`
   plus (RoF+ clients) `GUILD_ACTION_GUILD_CHAT_SEE` permission plus the
   client's own `FilterGuildChat` chat filter, then calls
   `client->ChannelMessageSend(from, to, chan_num, language, lang_skill,
   message)`.
7. **`Client::ChannelMessageSend`** (`EQEmu/zone/client.cpp:1675-`) builds a
   fresh `EQApplicationPacket(OP_ChannelMessage, ...)` — the exact same
   struct/opcode used for every other channel — and queues it to that
   client. This is what the RoF2 ENCODE at `rof2.cpp:1003` serializes onto
   the wire with `chan_num=0`.

So: **yes, definitively** — guild chat in RoF2 is `OP_ChannelMessage
chan_num=0` end to end, client and server, identical wire struct/opcode to
group (chan 2). The only differences from group chat are entirely
server-side: guild membership/permission checks and fan-out via
`SendPacketToZonesWithGuild` instead of `Group::GroupMessage`'s in-memory
group-member list. From the client's (eqoxide's) point of view, sending and
parsing guild chat is a pure mirror of group chat with `chan_num` swapped
0 <-> 2. No UCS/`OP_SetChatServer` plumbing is needed or used for guild.

## eqoxide status (as of #294)

Both directions were already implemented (matching the above) at the time
this note was written:
- Receive: `src/eq_net/packet_handler.rs:1408-1461` `apply_channel_message`
  maps `chan_num == 0 -> "guild"`, logs it, and emits an undirected
  `chat`/`guild` event. Covered by
  `apply_channel_message_guild_logs_and_events_as_guild` test
  (`packet_handler.rs:2613`).
- Send: `POST /v1/chat/guild {"text"}` in `src/http/chat.rs:53-59` pushes
  `ChatSend { chan: 0, to: "".into(), text }`, drained by the nav thread the
  same way as `/v1/chat/group` (`src/eq_net/navigation.rs:1587-1588`,
  `build_channel_message`).

## Gotchas / edge cases

- `targetname` on send is irrelevant for guild (server derives fan-out from
  `GuildID()`, not from the `to` field) — empty string is correct, same as
  group/shout/OOC.
- `language`/`skill_in_language` in the outgoing client packet are advisory
  only; the server recomputes `language_skill` from its own
  `m_pp.languages[]` table on receipt (`client_packet.cpp:4579-4582`) and
  ignores the client-sent skill value. Sending `language=0` (Common Tongue)
  like group chat does today is fine.
- `IsInAGuild()` / `GUILD_ACTION_GUILD_CHAT_SPEAK_IN` are the only gates; if
  eqoxide's test char isn't in a guild, the server replies with a
  `MessageString` (`GUILD_NOT_MEMBER2`) rather than relaying — that's a
  normal `OP_ChannelMessage`-adjacent server text message, not a protocol
  error, worth being aware of when writing an integration test against a
  live server.
- `ChatChannel_UCSRelay (20)` is the *only* channel number that goes to UCS
  (`EQEmu/world/zoneserver.cpp:429-432`); don't confuse it with guild (0).
  UCS is used for custom `/join` channels and cross-server mail, not guild.
