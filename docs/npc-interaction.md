# NPC Interaction: Hail, Say, Target, Consider

All NPC interaction goes through shared-arc slots that the nav thread drains each
tick (150 ms). The render HUD writes the same slots via its buttons and the HTTP
API writes them from external agents.

> **Combat, auto-grind, and buying** (auto-attack, auto-engage/retarget, the facing requirement,
> and merchant purchases) are covered in `autonomous-play.md`. This doc covers the
> hail/say/target/consider conversation flow.

---

## Flow Summary

```
Agent / HUD button
    ↓ sets Arc<Mutex<Option<…>>>
Navigator::tick() (every 150ms)
    ↓ drains the slot, builds packet, calls stream.send_app_packet(…)
EQEmu zone server
    ↓ OP_CONSIDER reply / NPC EVENT_SAY fires
packet_handler.rs
    ↓ parses reply → gs.log_msg() / gs.target_con = Some(color)
scene.rs (frame copy)
    ↓
hud.rs renders dialogue panel / tinted nameplate
```

---

## Hail

**Trigger**: `HailReq = Arc<Mutex<Option<String>>>` — set to the NPC's display name.

**How to invoke**:
- HTTP: `POST /v1/interact/hail {"name": "Guard Phaeton"}` or `{}` for nearest
- HUD: "Hail \<name\>" button in the control bar

**What happens**:
1. `Navigator::tick()` takes the name, builds `"Hail, Guard Phaeton"`
2. Sends `OP_CHANNEL_MESSAGE` with `chan_num=8` (Say channel)
3. EQEmu delivers the say to NPCs within 200 units; fires `EVENT_SAY`
4. NPC replies with `OP_SPECIAL_MESG` (opcode `0x2372`)
5. `packet_handler.rs: handle_special_mesg()` adds it to `gs.messages` with `kind="npc"`
6. `draw_quest_dialogue()` displays it in the NPC Dialogue panel

---

## Say (quest keyword follow-up)

**Trigger**: `SayReq = Arc<Mutex<Option<String>>>` — set to the text to say.

**How to invoke**:
- HTTP: `POST /v1/interact/say {"text": "shipment"}`
- HUD: type in the Say box and press Enter or "Send"
- HUD: click a `[keyword]` in the NPC Dialogue panel (auto-strips brackets)

**What happens**: same as Hail but with arbitrary text. No "Hail, " prefix.

---

## Target + Consider

**Trigger**: `TargetReq = Arc<Mutex<Option<u32>>>` — set to the spawn_id.

**How to invoke**:
- HTTP: `POST /v1/combat/target {"id": 1234}`
- HUD: "Target nearest" button

Also `POST /v1/combat/target/name {"name": "..."}` resolves a fuzzy name → spawn_id first.

**What happens**:
1. `Navigator::tick()` takes the spawn_id
2. Sends `OP_TARGET_MOUSE` (0x6c47, 4 bytes: spawn_id LE) — sets the server-side combat target
   (`GetTarget()`); this is what melee/auto-attack swings at
3. Immediately sends `OP_CONSIDER` (28 bytes: player_id + target_id + zeroes)
4. EQEmu replies with `OP_CONSIDER` carrying faction + level + con color
5. `packet_handler.rs: apply_consider()` stores `gs.target_id` and `gs.target_con`
6. `draw_labels()` tints the target nameplate with the consider color

---

## Consider Colors

From `ConsiderColor` enum (EQEmu source):

| Value | Meaning | RGB |
|-------|---------|-----|
| 1 | Ally | green |
| 2 | Warmly | green |
| 3 | Kindly | green |
| 4 | Amiably | light green |
| 5 | Indifferent | white |
| 6 | Apprehensive | yellow |
| 7 | Dubious | orange |
| 8 | Threatening | red |
| 9 | Deathly Afraid | red |

The color is stored in `gs.target_con: Option<[u8; 3]>` (RGB bytes) and mirrored
to `scene.target_con`. `draw_labels()` applies it to the target nameplate only.

---

## NPC Dialogue Panel

`hud.rs: draw_quest_dialogue()` renders messages with `kind == "npc"` that are
less than 45 seconds old. It disappears when there are no recent NPC messages.

`[bracketed]` keywords are rendered in gold and are **clickable**. Clicking one:
1. Strips the `[` and `]` brackets
2. Sets `SayReq` to the keyword text
3. Nav thread sends it as a Say packet on next tick

This lets an agent script the entire quest conversation via frame capture + HTTP.

---

## GM Debug Spam

If the logged-in account is GM-flagged, EQEmu sends loot-table debug lines as
NPC text via `OP_FORMATTED_MESSAGE`:

```
[Loot] AddLootDrop: item_id=1234 min/max=1/1
```

`is_debug_spam()` in `packet_handler.rs` filters these out before they reach the
NPC Dialogue panel. Matching strings: `"AddLootDrop"`, `"min/max"`, `"[Loot]"`.

---

## Quest Conversation Example

```sh
# Walk to an NPC
curl -X POST localhost:8765/goto -H 'Content-Type: application/json' \
     -d '{"name":"Lanhern Firepride"}'
sleep 6

# Hail it
curl -X POST localhost:8765/hail -H 'Content-Type: application/json' \
     -d '{"name":"Lanhern Firepride"}'
sleep 2

# Capture what it said
curl -s localhost:8765/frame -o /tmp/dialogue.png

# Follow up the [shipment] keyword
curl -X POST localhost:8765/say -H 'Content-Type: application/json' \
     -d '{"text":"shipment"}'
```
