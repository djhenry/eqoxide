# Cross-zone tells / OOC not delivered — RoF2 OP_ChannelMessage wire format was wrong (RESOLVED)

**Summary:** Cross-zone (and in fact *all*) chat sent via `OP_ChannelMessage` was silently dropped
by the server because the client encoded the **Titanium fixed-layout `ChannelMessage_Struct`**
(`targetname[64] | sender[64] | …`), but RoF2 uses a completely different **variable-length,
NUL-terminated** wire format. The server's RoF2 `DECODE(OP_ChannelMessage)` read our 64-byte padded
fields as garbage → empty target + wrong `chan_num` → the tell/OOC was never routed.

> **Note:** the original diagnosis below (cross-zone chat needs the UCS server) was **wrong**.
> Cross-zone **tells** and server-wide **OOC/auction** route through the **world server**, not UCS.
> UCS is only for named/numbered chat channels (`/join`), mail, and the buddy list. See the
> "Actual root cause" section.

**Severity:** Medium (blocked the cross-zone GM-helps-player use case). **Status: FIXED.**

## Steps to reproduce (pre-fix)
1. Launch two clients as different characters in **different zones** (e.g. Claude in `arena`,
   Durgan in `kaladimb`), each with its own `--api-port`.
2. `POST /tell {"to":"Claude","text":"..."}` on Durgan's client.
3. `GET /events` / `GET /messages` on Claude's client → nothing arrives.

## Actual root cause (2026-06-28)
RoF2's `OP_ChannelMessage` is **not** the Titanium `ChannelMessage_Struct`. Per EQEmu
`common/patches/rof2.cpp` `ENCODE`/`DECODE(OP_ChannelMessage)`, the wire format (both directions) is:

```
sender\0 | target\0 | u32 unknown | u32 language | u32 chan_num
         | u32 unknown | u8 unknown | u32 skill_in_language | message\0
```

(note: **sender first**, then target — the opposite order of the Titanium struct, and all
variable-length cstrings rather than fixed 64-byte fields).

The client was sending the Titanium struct, so the server's DECODE parsed `sender="Claude"` (our
targetname field), `target=""` (the NUL padding), and a garbage `chan_num` from the middle of the
sender padding. With an empty target and wrong channel, `Client::ChannelMessageReceived`
(zone/client.cpp) never relayed the tell. The **incoming** parser (`apply_channel_message`) had the
same fixed-struct assumption, so even server→client chat would have been misparsed.

### How the cross-zone routing actually works (no UCS involved)
- **Tell** (chan 7) → `worldserver.SendChannelMessage(…, ChatChannel_Tell, …)` → world
  `ClientList::FindCharacter(deliverto)` → delivers a normal `OP_ChannelMessage` to the recipient's
  zone (`world/zoneserver.cpp` `ServerOP_ChannelMessage`).
- **OOC / Auction** (chan 5 / 4) → if `ServerWideOOC` / `ServerWideAuction` rule is on →
  `worldserver.SendChannelMessage` (world broadcast); otherwise local-zone only.
- **Shout** (chan 3) → local zone only (`entity_list.ChannelMessage`); cross-zone shout doesn't exist.
- **UCS** only handles `OP_MailLogin` + `OP_Mail` (named channels via `/join`, mail, buddy list).

## The fix
`src/eq_net/navigation.rs::build_channel_message` and
`src/eq_net/packet_handler.rs::apply_channel_message` now emit/parse the RoF2 variable-length
format (with a `read_cstr` helper). Unit tests updated to the RoF2 layout.

## Verified live (2026-06-28)
Two clients, Claude in `arena` + Durgan in `kaladimb`:
- Durgan → Claude tell → Claude `/events`: `{"channel":"tell","directed":true,"from":"Durgan",…}` ✓
- Claude → Durgan tell → Durgan `/events`: `{"channel":"tell","directed":true,"from":"Claude",…}` ✓
- Claude OOC → Durgan `/events`: `{"channel":"ooc","directed":false,"from":"Claude",…}` ✓ (cross-zone)

## Follow-ons (separate, lower priority)
- **UCS link** (the `worktree-ucs-chat-link` step 1 `OP_SetChatServer` parse): still valid, but only
  buys **named/numbered chat channels + mail + buddy list**, not tells/OOC. Build only if those are
  wanted. Note the UCS port advertised by this server (`7778`) is outside the eqemu container's
  mapped UDP range (`7000-7400`) — would need a port-map change to connect.
- **Cosmetic:** the server's RoF2 `DECODE` does `emu->sender = Target`, and the server also sends a
  `TellEcho` of our own outgoing tell, so a sender currently sees both the local "You tell X: …"
  echo and a server `<Self> …` line. Harmless duplicate; de-dupe later if it's noisy.
