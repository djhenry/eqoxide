# eqoxide HTTP API

The agent-facing REST API the eqoxide client serves on `http://127.0.0.1:<port>`. Discover the port
from the `API_PORT=<N>` line the client logs on startup (it scans up from 8765, or binds the exact
`--api-port`). See `.claude/skills/build-run` for launch/port details.

## Versioning & grouping

All routes are **versioned and grouped**: `/<version>/<group>/<action>`. The current version is
**`v1`**. Groups mirror the agent's mental model (and the `eqoxide_mcp` tool grouping):

| Group | Purpose |
|-------|---------|
| `observe`   | read-only world/player state (incl. the `/v1/observe/frame` screenshot) |
| `navigate`  | movement: goto / warp / zone cross |
| `combat`    | targeting, auto-attack, consider, spell scribe/memorize/cast |
| `interact`  | hail, say, loot, give (turn-in), doors, sit/stand |
| `merchant`  | open/close a vendor, list wares, buy, sell |
| `inventory` | inventory management actions |
| `events`    | read the async event feed (chat/combat/navigate/system) |
| `chat`      | send messages on the inter-agent channels (tell/ooc/shout/group) |
| `camera`    | get/set the orbit camera |
| `lifecycle` | session control: camp / exit |

The `/v1` prefix exists so a future breaking revision can ship as `/v2` while old integrations keep
working. The implementation lives in `src/http/<group>.rs`, each exposing a `router()` that
`spawn_camera_server` nests under `/v1/<group>`.

---

## `observe` — read-only state

| Route | Description |
|-------|-------------|
| `GET /v1/observe/debug` | Player (zone, race, class, level, pos `[east,north,up]`, heading ccw/cw, `currency`, server_corrections) + camera state. |
| `GET /v1/observe/frame` | Current rendered frame as a PNG (`Content-Type: image/png`). |
| `GET /v1/observe/entities` | `{ "<name>": [x,y,z], ... }` for all known entities. |
| `GET /v1/observe/inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency}`. Slots are Titanium **wire** ids (DB general slots 23-30 → wire 22-29). |
| `GET /v1/observe/messages[?kind=npc]` | Machine-readable message log (oldest→newest). Each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `GET /v1/observe/spells` | The 9 memorized gems `{gem, spell_id, name}` (empty = null). |
| `GET /v1/observe/doors` | Current zone's doors `{door_id,name,x,y,z,heading,opentype,is_open}`. |
| `GET /v1/observe/zone_points` | Zone exit points received from the server. |
| `GET /v1/observe/quests` | "Quests near me": old-style Lua turn-in givers in this zone with distance, loaded flag, wanted items, reward XP. |
| `GET /v1/observe/quests/log` | The native EQ Task journal (server-pushed) with objectives + live progress. |

---

## `navigate` — movement

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/navigate/goto` | `{"name":"Guard Phaeton"}` \| `{"x":,"y":,"z":}` \| `{"map_x":,"map_y":}` | Walk to an entity (fuzzy name) or coordinates. `map_*` are Brewall map coords (= negated server x/y). |
| `POST /v1/navigate/warp` | `{"x":,"y":,"z":}` | Teleport to exact coords, bypassing collision. |
| `POST /v1/navigate/zone_cross` | `{"zone_id":N}` \| `{}` | Warp to a zone line and send OP_ZoneChange (specific zone, or nearest line). |

---

## `combat`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/combat/target` | `{"id":<spawn_id>}` | Target a spawn + auto-consider it. |
| `POST /v1/combat/target/name` | `{"name":"a rat"}` | Target a mob by fuzzy name. |
| `POST /v1/combat/attack` | — | Enable auto-attack. |
| `DELETE /v1/combat/attack` | — | Disable auto-attack. |
| `POST /v1/combat/consider` | `{"id":N}` (default current target) | Consider a spawn (con color/faction). |
| `POST /v1/combat/cast` | `{"gem":0-8}` \| `{"spell_id":N,"target_id":M?}` | Cast a memorized gem (on target, else current, else self). |
| `POST /v1/combat/memorize` | `{"spell_id":N,"gem":0-8}` | Memorize a known spell into a gem. |
| `POST /v1/combat/scribe` | `{"spell_id":N,"slot":B?}` | Scribe a spell scroll into the spellbook. |

---

## `interact`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/interact/hail` | `{"name":"NPC"}` \| `{}` | Say "Hail, <name>" so an NPC fires its hail/quest script (nearest if no name). |
| `POST /v1/interact/say` | `{"text":"..."}` | Say arbitrary text on Say (quest keyword follow-ups). |
| `POST /v1/interact/loot` | `{"id":N}` \| `{"name":"..."}` \| `{}` | Loot a corpse (specific id, fuzzy name, or nearest). |
| `POST /v1/interact/give` | `{"npc":"Name","from":N}` | Hand inventory slot N to an NPC (quest turn-in trade flow). |
| `POST /v1/interact/click_door` | `{"door_id":N}` \| `{"name":"DOOR1"}` | Click a door (server-authoritative open). |
| `POST /v1/interact/sit` | — | Sit (regen). |
| `POST /v1/interact/stand` | — | Stand. |

---

## `merchant`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/merchant/open` | `{"merchant":"Name"}` | Open a vendor window (OP_ShopRequest). Watch `list.open` for the result. |
| `POST /v1/merchant/close` | — | Close the merchant window. |
| `GET /v1/merchant/list` | — | The open merchant's wares `{open, merchant_id, count, items:[{merchant_slot,item_id,name,icon,price,quantity}]}`. |
| `POST /v1/merchant/buy` | `{"merchant":"Name","slot":N}` | Open the merchant and buy item slot N. |
| `POST /v1/merchant/sell` | `{"merchant":"Name","slot":N,"quantity":Q?}` | Sell player inventory slot N (qty default 1). |

> Note: the old flat aliases `/v1/merchant/buy`, `/v1/merchant/sell`, `/trade/*` are gone — use the `/v1/merchant/*` paths.

---

## `inventory`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/inventory/move` | `{"from":N,"to":M}` | Move/equip/unequip an item between slots (0-21 worn, 22-29 general, 30 cursor, 251+ bag). Reads live under `GET /v1/observe/inventory`. |

---

## `events` — the async event feed

The bus an agent polls for "what just happened, as soon as it happened". Every event is
`{id, category, kind, directed, from, text}`:

- `id` — **1-based** monotonic cursor. Pass the response's `last_id` as your next `?since=`.
- `category` — top-level bucket: `chat` | `combat` | `navigate` | `system`.
- `kind` — sub-type within the category (e.g. chat→tell/ooc/shout/group/gmsay, navigate→zone,
  combat→slain/attacked).
- `directed` — concerns *you* specifically (a /v1/chat/tell to your name, a GM message, your own zone change
  or death).

| Route | Query | Description |
|-------|-------|-------------|
| `GET /v1/events/all` | `?since=<id>&wait=<secs>&directed=1` | All events. |
| `GET /v1/events/<category>` | same | Only one bucket, e.g. `GET /v1/events/combat`, `GET /v1/events/navigate`, `GET /v1/events/chat`. Unknown categories return nothing. |

`?wait=<secs>` long-polls up to ~30s for a matching event (loop it to "listen" without busy-polling);
`?since=<id>` returns only newer events; `?directed=1` filters to events addressed to you.

Currently emitted: **chat** (incoming tells/ooc/shout/group/gmsay), **navigate** (`zone` — entered a
zone, incl. server-initiated changes / cross-zone respawns), **combat** (`slain` — you died;
`attacked` — a new mob started hitting you). More `kind`s land here over time without changing the
shape.

---

## `chat` — send on the inter-agent channels

(The *incoming* side is the read-only `events` feed above.)

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/chat/tell` | `{"to":"Name","text":"..."}` | Directed whisper (chan 7). The recipient sees a `directed` chat event. |
| `POST /v1/chat/ooc` | `{"text":"..."}` | Zone-wide OOC broadcast (chan 5). |
| `POST /v1/chat/shout` | `{"text":"..."}` | Zone-wide shout (chan 3). |
| `POST /v1/chat/group` | `{"text":"..."}` | Group-channel message (chan 2). |

---

## `camera`

| Route | Body | Description |
|-------|------|-------------|
| `GET /v1/camera` | — | Current orbit camera (azimuth, elevation, radius, focus, mode). |
| `POST /v1/camera` | `{"azimuth":,"elevation":,"radius":,"focus":[x,y,z]}` (all optional) | Set the orbit camera. |
| `POST /v1/camera/reset` | — | Reset to the default follow view. |

---

## `lifecycle`

| Route | Description |
|-------|-------------|
| `POST /v1/lifecycle/camp` | Toggle a camp (start, or cancel one in progress). A completed camp shuts the client down cleanly with no linkdead. |
| `POST /v1/lifecycle/exit` | Camp out (idempotent `Start`), then cleanly shut the process down (~30s). |

---

## Notes

- **Most actions are fire-and-forget**: a handler writes a shared request slot that the navigation
  thread drains each tick. The HTTP 200 means *queued*, not *done* — observe the result via
  `GET /v1/observe/*` or the `chat/events` feed.
- **Async travel**: `goto` / `zone_cross` return immediately; poll `GET /v1/observe/debug` (or watch
  for a `zone` event) to know when movement / a zone-in completed.
- **Coordinates**: server convention is `x=east, y=north, z=up`. Brewall map coords negate x/y.
- See `docs/autonomous-play.md` for end-to-end play recipes.
