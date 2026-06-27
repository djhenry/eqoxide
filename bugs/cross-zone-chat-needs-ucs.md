# Cross-zone tells / OOC not delivered — RoF2 routes them through the UCS chat server

**Summary:** The new chat commands send `OP_ChannelMessage` to the zone server, which works for
same-zone, zone-routed chat (say, and same-zone tells). But **cross-zone** tells (and OOC/auction)
under RoF2 are routed through the separate **Universal Chat Service (UCS)** server, which the
eqoxide client does not connect to — so an agent in zone A whispering an agent in zone B is never
delivered.

**Severity:** Medium (blocks the cross-zone GM-helps-player use case; same-zone chat works).

## Steps to reproduce
1. Launch two clients as different characters in **different zones** (e.g. Claude in `arena`,
   Durgan in `kaladimb`), each with its own `--api-port`.
2. `POST /tell {"to":"Claude","text":"..."}` on Durgan's client.
3. `GET /events` on Claude's client.

## Expected
Claude's `/events` shows a `directed` tell from Durgan.

## Actual
Nothing arrives. Each client only sees its **own** outgoing tell (the local echo
`"You tell X: ..."`). The recipient's `/messages` and `/events` never show the incoming tell.

## Diagnosis notes (2026-06-27)
- The outgoing packet format is correct: the `ChannelMessage_Struct` is **byte-identical** between
  Titanium and RoF2 (`targetname[64]` @0, `sender[64]` @64, `chan_num` @132, `message` @148 — see
  `EQEmu/common/patches/{titanium,rof2}_structs.h`), and `/say` (chan 8) works (NPCs respond), so
  the server reads our `chan_num` + message fine.
- Incoming `OP_ChannelMessage` parsing works: live NPC `say` dialogue shows in `/messages`, and the
  tell classification is unit-tested in `apply_channel_message`.
- The server is running a **UCS** server (`/opt/eqemu/data/logs/ucs.log`). In RoF2, cross-zone
  tells, OOC, and auction route through UCS (a distinct TCP connection + `OP_Mail`-family opcodes),
  not the zone's `OP_ChannelMessage`. The client never connects to UCS, so those messages are
  dropped on the routing side.

## Suspected root cause
The client doesn't establish the UCS (mail/chat) connection that RoF2 uses for cross-zone chat.
Same-zone messages are delivered by the zone server directly, so they work; cross-zone needs UCS.

## What already works (this worktree's feature)
- `POST /tell|/ooc|/shout|/group` (verified: packets sent, local echo confirms).
- `GET /events` structured feed + long-poll + `directed` flag (the "for me" awareness API).
- Incoming tell/ooc/shout/group/gmsay classification → events (unit-tested).

## Fix sketch (follow-on)
Connect to the UCS server during login (world handshake gives the UCS host/port), authenticate, and
send/receive tells + OOC over it (`OP_Mail` / chat opcodes). Then route `/tell`/`/ooc` through UCS
when the target isn't in our zone, and ingest UCS chat into the same `chat_events` feed.

## Status
In progress — branch `worktree-ucs-chat-link`.
