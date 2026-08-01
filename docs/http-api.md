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
| `move`      | movement: goto (walk & stop) / follow (walk & keep following) / stop / zone cross |
| `combat`    | targeting, auto-attack, consider, spell scribe/memorize/cast |
| `interact`  | hail, say, loot, give (turn-in), doors, sit/stand |
| `quests`    | native EQ task journal (server-pushed), task offers/accept/decline/cancel |
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
| `GET /v1/observe/debug` | Player (zone, race, class, level, pos `[east,north,up]`, heading ccw/cw, `currency`, server_corrections, vitals `hp_pct`/`hp`/`hp_max`/`mana_pct`/`xp_pct`, `levitating` (three-valued `true`/`false`/`null` — see [`levitating`](#levitating--three-valued-levitate-buff-state-not-a-gravity-reading-598)), target `target_id`/`target_name`/`target_hp_pct`/`target_con`/`target_attitude`/`target_level`) + **navigation — SPLIT ACROSS TWO NESTING LEVELS; this grouping is by topic, not by where the field lives** (under `player`: `nav_state`, `nav_reason`, `position_provisional`, `crossing_pending_ms`. Top-level, siblings of `player`, NOT under it — same convention as `last_consider`: `nav_goal_id`, `nav_goal`, `nav_blocked_by`, `nav_tier`, `nav_declined_pads`, `nav_local`, `nav_local_planner_dead`, `nav_support`, `nav_tight`; they sit outside `player` because that object is already at serde_json's macro recursion limit — see [Navigation state](#navigation-state), [The fine steering tier](#the-fine-steering-tier-nav_local--382) for `nav_local` and [`nav_local_planner_dead`](#nav_local_planner_dead--fine-planner-liveness-session-scoped) — the **session-scoped** fine-planner liveness flag, the one nav field that is always present rather than `null` when healthy, and the one to poll for a dead fine planner because `nav_local` retires with the goal — and [`nav_declined_pads`](#nav_declined_pads--the-teleport-pads-nav-refused-offered-back-to-you-543--266)) + **connection health** (`connected`, `link_age_ms`, `last_packet_age_ms`, `snapshot_age_ms`, `world_responsive`, `last_world_response_ms`, `send_failures`, `send_wouldblock_rescued`, `send_deferred`, `send_starved`, `send_failures_unretried`, `last_send_error`, `last_send_error_age_ms`, `reliable_abandoned` — see [Connection health](#connection-health)) + **`net_thread_dead`** (`null` while the network thread is alive; a reason string once it has died and the whole payload is a frozen final snapshot — see [net_thread_dead](#net_thread_dead--the-frozen-worlds-terminality-634)) + **`zone_cross_best_effort`** and **`zone_cross_stopped`** (top-level, `null` while there is nothing to disclose — see [Zone-cross degradations you can detect](#zone-cross-degradations-you-can-detect-713)) + **`last_consider`** (spawn-scoped result of the most recent consider of ANY spawn, target or not — see [Consider results](#consider-results)) + camera state. |
| `GET /v1/observe/frame` | Current rendered frame as a PNG (`Content-Type: image/png`). **503 while the zone's assets are still loading** — see [`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579); `?allow_pending=1` opts past it. Optional `preset`/`pitch`/`yaw`/`distance` params request a one-off diagnostic camera angle for just this capture — see [Camera override for `/frame`](#camera-override-for-observeframe-422). |
| `GET /v1/observe/entities[?labeled=1]` | Default: `{ "<name>": [x,y,z], ... }` for all known entities, with same-base-name + byte-identical-position duplicates collapsed (#471 — suspected server-side `spawn2` duplication; the model is untouched so each instance is still targetable by its full name). `?labeled=1` returns the richer `{count, entities:{"<name>":[x,y,z]}, deduped, duplicate_groups:[{position,names,kept}], note, poses, snapshot_age_ms}` exposing which duplicates were collapsed, plus **`poses`** (#643): `{"<name>": {pose, gait}}`, keyed **exactly** like `entities` — the two are projected under one lock, so indexing `poses` by any name in `entities` is safe. `pose` is the server-published body state — `standing`/`freeze`/`looting`/`sitting`/`crouching`/`lying`, or **`unknown(<raw>)`** when the server sent a code this client does not recognise (reported verbatim, never guessed at). `gait` is the signed locomotion-speed code from the entity's last position update (~12 at walk, 28 at full run, negative when backing up); **`null` means "no position update yet", NOT "standing still"**. The default bare-map shape carries the same freshness value in the `X-Snapshot-Age-Ms` header instead — see [Per-endpoint freshness](#per-endpoint-freshness--snapshot_age_ms-646). |
| `GET /v1/observe/inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency, coin_verified, snapshot_age_ms}`. Slots are Titanium **wire** ids (DB general slots 23-30 → wire 22-29). |
| `GET /v1/observe/messages[?kind=npc]` | Machine-readable message log (oldest→newest). `{count, messages, snapshot_age_ms}`; each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `GET /v1/observe/dialogue` | Pending NPC dialogue/quest choices `{count, choices:[{index, text}], snapshot_age_ms}`. |
| `GET /v1/observe/spells` | The 9 memorized gems `{gems:[{gem, spell_id, name}], snapshot_age_ms}` (empty = null). |
| `GET /v1/observe/skills` | All skills with current trained value `{skills:[{id, name, value}], snapshot_age_ms}`; `value == 0` means untrained. |
| `GET /v1/observe/doors` | Current zone's doors — a bare array `[{door_id,name,x,y,z,heading,opentype,is_open}]`; freshness rides the `X-Snapshot-Age-Ms` header (no room for a JSON key on a bare array). |
| `GET /v1/observe/zone_entrances` | Zone entrance points received from the server (arrival side — see [Navigation state](#navigation-state) for the distinction from `zone_exits`). Also served at the deprecated alias `GET /v1/observe/zone_points`. A bare array; freshness rides the `X-Snapshot-Age-Ms` header. |
| `GET /v1/observe/zone_exits` | Current zone's exits (the WLD zone-line regions you navigate toward — see [`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579) for its 503 gating). An entry with `"zone_id": null` is a REAL exit region whose baked index matches no advertised zone point (#683) — its destination is honestly unknown until the server resolves a crossing there. Crossing it works **only if this zone advertises no same-zone teleport point and server zone points are available** (received and not all filtered client-side) — the same #679 gate that keeps the client from firing a blind crossing off an intra-zone pad. In a gated zone (e.g. one with teleport pads), standing on a `zone_id: null` exit does NOT cross. **Each entry carries `"gated": true|false` (#713)** — it reports the **#679/#683 zone-level** unresolved-cross gate and nothing else, so you can see that refusal **before** walking there instead of reading about it in the message log after. **`gated` is a property of the zone and the entry, not of you**: your position is not an input to it. `gated` is only ever `true` for `zone_id: null` entries (an advertised destination is crossed directly and is never subject to the #679 gate); it **reports** the gate verdict and does not change it. **`gated: false` is not a promise the auto-cross will fire.** It says only that this gate is open — the stand-scoped [#713 attempt bound](#zone-cross-degradations-you-can-detect-713) can independently have stopped auto-crossing, and `zone_exits` never consults it, so cross-check `zone_cross_stopped` on `/v1/observe/debug` before concluding an exit is broken. The message-log line ("auto-cross is disabled here", once per stand) is still emitted for callers that watch events. A bare array; freshness rides the `X-Snapshot-Age-Ms` header. |
| `GET /v1/observe/item_text` | Text of the most recently read book/note `{text, snapshot_age_ms}` (`text: null` if none read this session). |
| `GET /v1/observe/packets[?summary=1]` | Packet-telemetry ring dump (#525), default-off capture. `{enabled, count, packets, snapshot_age_ms}`, or with `?summary=1`, `{enabled, summary, snapshot_age_ms}` (opcode histogram + reliable-sequence-gap analysis). |
| `GET /v1/observe/who` | Server-wide `/who all` roster `{online:[{name, level, class, race, zone_id, guild, anon}], snapshot_age_ms}`. 503 if no response arrives in time. |
| `GET /v1/observe/nav_debug` | The nav diagnostics snapshot navigation **publishes** (#608) — see [Nav diagnostics](#nav-diagnostics-get-v1observenav_debug--608). |
| `GET /v1/observe/asset_sync` | Every asset-server activity in flight (#715, #731) — phase, chunks, bytes and download rate while the client downloads a zone's (or the common) asset set, plus the logins that precede them. `{"active": false}` when nothing is running. See [Asset sync progress](#asset-sync-progress-get-v1observeasset_sync--715). |

Every route above that lacked ANY freshness signal before #646 now carries one — either a
top-level `"snapshot_age_ms"` JSON field or, where the body is a bare array/map/PNG that cannot
safely gain a new key, the `X-Snapshot-Age-Ms` response header. See
[Per-endpoint freshness](#per-endpoint-freshness--snapshot_age_ms-646) for the full field-vs-header
breakdown and why.

---

## `move` — movement

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/move/goto` | `{"name":"Guard Phaeton"}` \| `{"x":,"y":,"z":}` \| `{"map_x":,"map_y":}` \| `{}` | Walk to an entity (fuzzy name, one-time snapshot) or coordinates and **stop** on arrival. Empty body → the player's current target. `map_*` are Brewall map coords (= negated server x/y). **Returns JSON**, including [`matched`](#matched--which-entity-a-name-actually-resolved-to) when the goal came from a name/target. |
| `POST /v1/move/follow` | `{"name":"a rat"}` \| `{}` | Walk to a named entity and **keep following** it until canceled. Empty body → current target. Coordinates are rejected (400). **Returns JSON** with [`matched`](#matched--which-entity-a-name-actually-resolved-to). |
| `POST /v1/move/stop` | — | Cancel any active goto/follow. |
| `POST /v1/move/zone_cross` | `{"zone_id":N}` \| `{}` | Cross a zone line and send OP_ZoneChange (specific zone, or nearest line). |

---

## `combat`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/combat/target` | `{"id":<spawn_id>}` | Target a spawn + auto-consider it. |
| `POST /v1/combat/target/name` | `{"name":"a rat"}` | Target a mob by fuzzy name. **Returns JSON** with [`matched`](#matched--which-entity-a-name-actually-resolved-to) — always check it before acting on the target. |
| `POST /v1/combat/attack` | — | Enable auto-attack. |
| `DELETE /v1/combat/attack` | — | Disable auto-attack. |
| `POST /v1/combat/consider` | `{"id":N}` (default current target) | Consider a spawn (difficulty tier + faction attitude). Result: `target_con`/`target_attitude`/`target_level` on `/observe/debug` if the spawn IS the current target, always `last_consider` regardless — see [Consider results](#consider-results). |
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

## `quests`

| Route | Body | Description |
|-------|------|-------------|
| `GET /v1/quests/log` | — | The native EQ Task journal (server-pushed) — active tasks only, with objectives + live progress. |
| `GET /v1/quests/completed` | — | Completed task history: `{task_id, title, completed_time}[]`. |
| `GET /v1/quests/offers` | — | Pending task offers from an open selector window: `{task_id, npc_id, title, description, has_rewards}[]`. |
| `POST /v1/quests/accept` | `{"task_id":N}` | Accept one offered task. |
| `POST /v1/quests/decline` | — | Decline all pending task offers. |
| `POST /v1/quests/cancel` | `{"task_id":N}` | Abandon an active task. |

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
| `POST /v1/lifecycle/respawn` | Revive a slain character at its bind point. On death the client now HOLDS the character dead (no auto-respawn) so an agent can inspect `dead`/`killed_by` in `/v1/observe/debug` and recover its corpse; this releases it. No-op (still 200) if not currently dead. (#284) |

---

## Notes

- **Most actions are fire-and-forget**: a handler writes a shared request slot that the navigation
  thread drains each tick. The HTTP 200 means *queued and not overwritten*, not *done* — observe the
  result via `GET /v1/observe/*` or the `chat/events` feed. See
  [What a 200 means, and the two ways an action is refused](#what-a-200-means-and-the-two-ways-an-action-is-refused-347).
- **Async travel**: `move/goto` / `move/zone_cross` return immediately; poll `GET /v1/observe/debug` (or watch
  for a `zone` event) to know when movement / a zone-in completed.
- **Coordinates**: server convention is `x=east, y=north, z=up`. Brewall map coords negate x/y.
- See `docs/autonomous-play.md` for end-to-end play recipes.

### What a 200 means, and the two ways an action is refused (#347)

A `200` from an action endpoint asserts exactly two things, and no more:

1. the request passed the checks the client could make against its own published state, and
2. it reached the command slot **without displacing an earlier request that had not been sent yet**.

It does **not** assert the packet went out, and it never asserted the server accepted it. Anything
stronger has its own status code (`/merchant/{buy,open}`, `/interact/give` and `/combat/cast` await
the real outcome and answer `200` / `409` / `202 unconfirmed`).

**`404` — rejected at the door.** The client refuses a request its own published snapshot already
contradicts, instead of queueing it and answering `200`. Nothing is queued and nothing is sent:

| Route | Checked against | Refused when |
|-------|-----------------|--------------|
| `POST /v1/combat/target`, `POST /v1/combat/consider` | `GET /v1/observe/entities` | no spawn with that id is in the zone |
| `POST /v1/inventory/move` | `GET /v1/observe/inventory` | `from` holds no item |
| `POST /v1/interact/give` | `GET /v1/observe/inventory` | `from` holds no item |
| `POST /v1/merchant/sell` | `GET /v1/observe/inventory` | `slot` holds no item |
| `POST /v1/combat/scribe` | `GET /v1/observe/inventory` | `from` is supplied and holds no item |
| `POST /v1/interact/read` | `GET /v1/observe/inventory` | `slot` holds no item (`409` if it holds something unreadable) |

The snapshot these are checked against is the one the network thread publishes. It is refreshed on
every inbound packet, **and** — since #347 — immediately after the client mirrors a change the server
applies without echoing it (an item move: EQEmu sends no `OP_MoveItem` back for a move you asked
for). So a `/v1/inventory/move` that has already been drained is visible to the next door check
without waiting for a packet to arrive. It is still a snapshot: it cannot know about a change only
the server has made and not yet told the client about.

**`409` — the slot is busy.** Each action endpoint queues into a single-slot mailbox the net thread
drains in its next pass. That pass runs on the network loop's ~10 ms cadence, not on the ~150 ms
walker tick — of the drains, only `give` sits behind the walker gate (and behind the dead-player
early return, so while you are dead a queued `give` is not drained at all). A second request arriving
inside that window used to **replace** the first, and both callers were told `200` — one of the two
actions simply never happened and nothing
said so. It is now refused: the pending request is kept untouched and the second caller gets `409`
with a body ending `(it was NOT queued)`. A `409` is definitive — that request did not happen, so
retrying it after the drain is safe and cannot double-fire.

This applies to every verb under `/v1/combat`, `/v1/interact`, `/v1/inventory`, `/v1/merchant`,
`/v1/trainer`, `/v1/pet`, `/v1/group`, `/v1/quests` and `/v1/guild`. Three families are deliberately
exempt because a second request there is a *retarget*, not a lost action, and is already observable:
`/v1/move/*` (each write stamps a fresh `nav_goal_id` and republishes `nav_state`),
`/v1/lifecycle/*` (`camp` is a toggle; `exit` must be able to override an in-progress camp), and
`/v1/social/*` (a superseded reply channel closes and answers `503`). `/v1/chat/*` is an unbounded
FIFO and never dropped anything.

---

## Navigation state

`GET /v1/observe/debug` carries **`nav_state`** (what navigation is doing) and **`nav_reason`** (the
machine-readable *why*, `null` unless a state has one). Together they are how you find out whether a
`/v1/move/*` you fired actually worked — the 200 only means *queued*.

| `nav_state` | Meaning | `nav_reason` |
|-------------|---------|--------------|
| `pending` | A `/move/{goto,follow,zone_cross}` was **just accepted** and the walker has not ticked yet. Normally it lasts one walker tick (~150 ms) and becomes `planning`/`navigating`/`following`; a `/zone_cross` issued during a zone load holds it until the request is drained (see `zone_loading`). Its purpose is honesty: the instant a new request is accepted the state resets to `pending` (under a fresh `nav_goal_id`), so a read can **never** return the *previous* goal's terminal `arrived`/`no_path`/`blocked` as if it were the new request's outcome (#349). **`pending` always retires.** It is not on the terminal list, and every walker tick that finds no goal in flight and no queued `/zone_cross` retires any non-terminal state to `idle` with a reason — so `pending` cannot outlive the request that stamped it (#725; before that fix a dropped `/zone_cross` left `pending` standing indefinitely — measured at 75 s — with `nav_reason` and `nav_goal` both `null`). | — |
| `idle` | Nothing to do. **Every `idle` the client publishes after start-up carries a `nav_reason` saying how it got there**, so `nav_state: "idle"` with `nav_reason: null` means exactly one thing: no nav request has been made since this client started (#725). It is otherwise a real outcome, not an absence of one. | `zoned`, `stopped`, `goto_superseded`, `goal_dropped`, `respawned`, `zone_cross_dropped_unhandled` — all below; `null` **only** at start-up |
| `planning` | A route is being computed on the pathfinding worker thread. The character stands still. Normally < 1 s. | — |
| `navigating` | Walking a **complete route to your goal**. | `goal_z_snapped` (see below) or — |
| `navigating_partial` | Walking a **partial** route: the search was cut short, so this is *not* a route to your goal — it's progress toward a frontier, and it will re-plan from the far end. Usually resolves to `navigating` or `arrived`. | `search_node_cap` |
| `following` | A `/follow` chase has caught up; holding near the leader, still latched. | — |
| `arrived` | Reached the goal. | `goal_z_snapped` (see below) or — |
| `no_path` | **DEFINITIVE: no route exists.** The planner searched to completion. Do not retry the same goal — pick another. | see below |
| `search_exhausted` | The planner **gave up**. This is **"I don't know", not "no"** — a route may well exist. Try a nearer waypoint. | `search_node_cap` |
| `blocked` | A route exists, but the walker **could not follow it** (wedged after 8 recovery attempts). Not a routing failure. | `walker_stalled`, `local_no_way_through`, `fall_would_be_lethal` |
| `zone_loading` | **This client has no *usable* model of the zone the character is in yet** — its terrain/collision are still loading, their load failed, or the loaded grid still belongs to the zone the character just LEFT (the stale window, #600). No search was run and no route exists to report; the goal is kept and planned for real once the correct zone's assets land. Since #600 the walker refuses through the SAME `zone_assets::usability` predicate the HTTP world endpoints use, so the reason is that predicate's own verdict — read `zone_assets` (below) for the matching detail. | `zone_assets_pending`, `zone_assets_failed`, `zone_assets_idle`, `zone_assets_stale_for_previous_zone`, `player_zone_unknown` |
| `dead` | **The character is slain** — navigation was abandoned because a corpse cannot move (#238, #644). Terminal and honest: an agent that issued a goto and then polled must be able to tell "you died and went nowhere" from the ambiguous `idle` (which also means "ready for work"). Clears back to `idle` with `nav_reason: "respawned"` on respawn. **A movement command issued *while* dead is not accepted at all** — `POST /v1/move/{goto,follow,zone_cross,manual,jump}` returns **`409 Conflict`** with a machine token `dead` (JSON `"status":"dead"` on `/goto` and `/follow`; the text body names `dead` on the others), so you never get a `200 … navigating` for a goal a corpse can never reach. Respawn (`POST /v1/lifecycle/respawn`) before reissuing. | `player_dead` |

### Why an in-progress `nav_state` can never stick (#725)

Every `nav_state` is either **terminal** — `idle`, `arrived`, `no_path`, `search_exhausted`,
`blocked` — or **in progress**. The rule the walker applies each tick is stated over the terminal
set, not over a list of in-progress words: *if there is no goal in flight and no queued
`/move/zone_cross`, any state that is not terminal is retired to `idle` with a `nav_reason`.* So an
in-progress state is only ever published while something is genuinely happening, and a word that is
neither driven forward nor listed anywhere still retires — you do not have to trust that each state
remembered to clean itself up.

That rule replaced an opt-in list of states-to-retire, under which any state missing from the list
survived forever once its goal vanished. Two were missing, and both were observed live: `pending`
after a dropped `/zone_cross` (#725), and `following` after the followed entity despawned.

### Every `idle` says how it got there (#725)

`idle` used to be published bare from several different places, and one of them was the **success**
path of `/v1/move/zone_cross` — so a successful crossing and a request the client had thrown away
looked byte-identical to a polling agent (`"nav_state":"idle","nav_reason":null` in both). Each of
those call sites now names itself. The complete set of ways to reach `idle`:

| `nav_reason` | Meaning |
|--------------|---------|
| `zoned` | **The character changed zone**, and navigation was reset because a route computed in the old zone means nothing in the new one. This is the `nav_state` a *successful* `/v1/move/zone_cross` ends at — read it together with `player.zone`, which is the authoritative statement of where you are. It is deliberately about the zone change and not about the request, so it is equally true of a GM `#zone`, a gate/evac, or a portal door. Not an error. |
| `stopped` | **You asked** — `POST /v1/move/stop` was accepted and any goto/follow/queued zone-cross was cancelled. |
| `goto_superseded` | You did **not** ask: something else took over steering — manual movement (keyboard or `POST /v1/move/manual`), or the auto-melee-engage override. Your goto is gone; reissue it if you still want it. |
| `goal_dropped` | Your goal stopped existing without being reached — e.g. a `/follow` target despawned, or a request was cancelled from elsewhere in the client. Not an error about the route; there is simply nothing left to walk to. Reissue if you still want it. |
| `respawned` | The `dead` state cleared because the character came back up (#644). |
| `zone_cross_dropped_unhandled` | **A client bug, reported instead of hidden.** Your `/move/zone_cross` was consumed by the client and produced no outcome at all — no walk, no crossing, no refusal. Nothing is in flight and nothing will happen; retry, or use `/move/goto`. If you see this, please file it with the zone and your position: it means a code path took your request and wrote nothing, which is exactly the defect the backstop that emits this reason exists to make visible (#725). |

### `levitating` — three-valued levitate buff state, NOT a gravity reading (#598)

`player.levitating` reports whether the self-player currently has **Levitate** up (SPA 57 — gravity
off, the character free-floats instead of falling and holds altitude with no input). It is
**three-valued**, and the distinction is load-bearing for the agent-honesty invariant:

| Value | Meaning |
|-------|---------|
| `true`  | Levitating. `pos_up` is a height the character will **not** fall from, and the controller applies no gravity. |
| `false` | A **trustworthy** negative — the client has complete buff information and none of it is levitate. |
| `null`  | **UNKNOWN.** The client received a buff it could not resolve (its spell table — `spells_us.txt` — is missing or truncated) and no channel positively asserts levitate, so it genuinely cannot say. This is **never** silently reported as `false`. |

The key is **always present** (never omitted), so an absent key can never be mistaken for "known
false". Treat `null` as "I don't know", not as "no": if levitate gates a decision, and you read
`null`, gather more information (or ensure the client has a spell table) rather than assuming the
character is grounded.

**This is the Levitate *buff* state only — it is NOT a general "am I subject to gravity?" flag.**
It is derived from the two server channels that carry the *levitate buff*: the self-spawn `flymode`
byte / `OP_SpawnAppearance` type 19 (Levitating=2 / LevitateWhileRunning=5), and the buff list
cross-referenced to SPA 57. GM `#flymode 1` (Flying) genuinely turns gravity off yet reports
`false`, because #529 deliberately scoped this field to the levitate buff, not to every gravity-off
mode. An agent reasoning specifically about the levitate *buff* can trust it; an agent that wants a
general gravity answer must not read this field as one.

### `hold` — the character is physically stuck and the client cannot free it (#724)

`player.hold` is `null` for a healthy character — **including one that is simply standing still** —
and non-null only while the movement controller has stopped the body and has no way to resume.

It exists because those two states were indistinguishable through this API. `pos` is correct in
both. `nav_state` is `idle` in both. `nav_state.stuck_ticks` is the *walker's* counter and only
advances while a `/goto` is actively driving, so a character that was summoned into a rock and is
standing there produced **no observable at all** — every movement command returned `200`, nothing
moved, and every other field read normal.

```jsonc
"hold": {
  "reason":    "embedded_no_recovery",   // or "underworld_no_recovery"
  "held_secs": 12.4,                     // controller frame time, this unbroken hold
  "detail":    "…what is true and what you can do about it…"
}
```

| `reason` | What is true | Can the character move? |
|----------|--------------|-------------------------|
| `embedded_no_recovery` | The body is **embedded in world geometry**. The push-out search found nowhere it can legally stand, and there is no recovery position to fall back to (a position discontinuity — a GM summon, a large server correction — supersedes that history, #724). | **No.** Physics is frozen; every movement command is accepted and produces no motion in any direction. |
| `underworld_no_recovery` | The body fell to the zone's **underworld floor** and the client is holding it there rather than let it drop out of the world (#150), with no recovery position to restore. It is hanging: not falling, not landing, not grounded. | Horizontally, yes — but there is probably nothing under it. |

**Neither clears on its own.** The client goes on streaming its own (unchanged) position and the
server agrees with it, so no further server correction is coming. A GM `#goto`/`#summon`, or zoning
out, is what ends it.

**`held_secs` is controller frame time as of the last stepped frame, not wall clock since entry.**
A frozen body's meaningful clock is the physics clock. If the render loop is not stepping, no frames
elapse, `held_secs` stops advancing, and the `pos` beside it is stale by exactly the same amount.

**How to detect that — and what does *not* detect it.** Poll twice and compare the change in
`held_secs` against the wall-clock time between your two reads. If `held_secs` did not advance, the
controller is not stepping and every physics field in the payload is stale by that much. Do **not**
use the [connection health](#connection-health) block for this: every field there measures the
*network thread*, the link, or the world — `snapshot_age_ms` is milliseconds since the network
thread last ticked, and `connected` is derived at HTTP read time and needs no render at all — so a
stalled render loop with a live network reads as perfectly healthy there. Nothing in this API tracks
the render loop directly; `held_secs` against your own clock is the check. (#724 round-2 review, N3
— an earlier draft of this paragraph pointed at the health block, which structurally cannot answer
the question it was offered for.)

**`hold` lags the onset of an embedded freeze by up to 0.5 s.** `embedded_no_recovery` is raised
only after the push-out search has failed continuously for that long. During the lag the body is
already frozen — movement commands return `200` and produce no motion — but the client is still
retrying the push-out every frame and may yet succeed, so the lag is a genuine "still trying"
window rather than a silence. Read `"hold": null` as *no hold is in force*, never as *the body moved
this frame*. `underworld_no_recovery` has no such lag; it is raised on the first refused descent.
(#724 round-2 review, N5.)

**The key is always present** (never omitted), so an agent that greps for `hold` and finds nothing
knows it is talking to a client too old to report the state, rather than concluding all is well.
And it does not latch. On every **rendered** frame the controller recomputes it from scratch, so it
disappears the frame the body is freed. On the frames that render but do not **step** it is cleared
explicitly instead: for the whole ~10 s of a zone's asset load there is no collision to step against,
and a zone-in clears the mirrored copy, so a hold never survives into a zone the character has left.
If the render loop goes idle it stops recomputing altogether —
but a held body cannot be *freed* without a stepped frame either, so idling cannot manufacture a
false hold; what it freezes is `held_secs`, and the paragraph above tells you how to detect that.
(#724 round-3 review, N1 — this used to say "recomputes it from scratch every frame", which is not
true of the load, zone-in or idle paths.)

### `afloat_stall` — this swimmer is being asked to swim and is going nowhere (#776/#801)

`player.afloat_stall`, in the `player` object of **`GET /v1/observe/debug`**, is `null` for every
ordinary character, **including every ordinary swimmer**, and non-null only while the body is
*afloat*, *being wished at horizontally*, and *not getting anywhere*.

It exists because a genuinely trapped swimmer had **no observable at all**. A body afloat in water
never enters the client's depenetration net, so a swimmer sealed in a pocket or pressing at a
passage it cannot pass reads `on_ground: false`, `in_water: true`, `hold: null`, and a walker
`stuck_ticks` that only advances while a `/goto` is driving. Every field said "swimming normally",
which is the silent-wrong-answer class this project ranks above crashes.

```jsonc
"afloat_stall": {
  "secs":                 4.8,        // controller frame time, this unbroken stall
  "anchor_east":         -161.2,      // the point it has failed to get away from…
  "anchor_north":         842.7,      // …same frame and datum as pos_east/north/up
  "anchor_up":            -18.0,
  "stall_threshold_secs": 3.0,        // engineering choices, NOT measurements
  "progress_threshold":   0.5,
  "detail":               "…what is true and what you can do about it…"
}
```

**This is not a `hold`, and the difference is actionable.** A [`hold`](#hold--the-character-is-physically-stuck-and-the-client-cannot-free-it-724)
claims *the body cannot move at all, under any driver* — the only ways out are a GM or a zone. An
`afloat_stall` claims only that *the wish currently being made is producing no motion*. The worked
case is a submerged pocket mouth: it stalls a horizontal swim wish indefinitely and is still
escapable by a **driven dive**. So when you see one, try a vertical wish, try backing out the way you
came, try a different heading — and only then treat it as a genuine trap.

| Field | What it is |
|---|---|
| `secs` | How long the stall has been continuously in force, in **controller frame time** as of the last stepped frame — the same clock and the same caveat as `hold.held_secs`. It counts the whole window including the part before the threshold, so it is the true age of the stall and is always at least `stall_threshold_secs`. |
| `anchor_*` | The position the window opened at: the point the body has failed to get more than `progress_threshold` away from, in any direction. Same coordinate frame and FOOT datum as `pos_east`/`pos_north`/`pos_up`, so you can difference them directly. |
| `stall_threshold_secs`, `progress_threshold` | The two thresholds this report was produced against, published so you do not have to guess them. |

**`null` does NOT mean "not stuck".** The predicate is deliberately narrow, because a false alarm in
an honesty observable is the same defect as a silence — and the naive "stationary and wet" test
fires on every floating character alive. These bodies are genuinely trapped and are reported `null`:

* a swimmer **slowly losing ground**, or drifting — progress is measured as net displacement from
  the anchor, not as progress toward any goal, so a body that creeps more than `progress_threshold`
  in *any* direction re-anchors and the clock restarts. Backwards counts as progress here;
* a swimmer **circling a pocket wider than `progress_threshold`** — it re-anchors every lap. Progress
  is measured in 3-D, so a body oscillating vertically through more than the threshold re-anchors
  too;
* a swimmer lidded under a **purely vertical wish** with no horizontal component — no window ever
  opens. The wish half of the predicate is horizontal-only on purpose: a sustained up-wish at the
  surface is exactly what a legitimate haul-out does, and it is the single most common wish in the
  water system, so counting it would false-alarm constantly;
* any **dry** body pressed against a wall, and any body **wading on the bottom** — both out of scope;
  the depenetration net's `hold` vocabulary owns them.

**The thresholds are engineering choices, not measurements.** `3.0 s` is sized to be far longer than
any legitimate transient a floating body has (buoyancy settles in ~0.07 s; a swimming step-up or
duck-under resolves on the frame it is tried or never). `0.5 u` is ~0.011 s of travel at the speed
the nav swim drive uses, so a body genuinely swimming clears it on its first frame. Neither number
was tuned against measured data, and this document does not claim otherwise.

**The key is always present in `GET /v1/observe/debug`'s `player` object** (never omitted), so an
agent that greps that response for `afloat_stall` and finds nothing knows it is talking to a client
too old to report the state, rather than concluding the swimmer is fine. Two things that sentence
does **not** say, because #801's round-1 review caught it saying them: it is a claim about *that one
route*, not about the API in general — no other endpoint carries the field, and there is no bare
`GET /v1/observe` or `/v1/observe/state` route to carry it; and it is a claim about a served response
body, which is a different and stronger thing than the field existing on an internal struct. It was
false when first written for exactly that reason: six files of the publication path were correct, the
Rust type was populated, and no handler ever serialised it, so the key was present in nothing. What
makes it true is one `player.insert("afloat_stall", …)` in `crates/eqoxide-http/src/observe.rs`'s
`get_debug`, and one test that reads it back out of a real response through the real router. It does
not latch: the controller recomputes it on every stepped frame, the render
thread republishes it on every rendered frame in the same statement that republishes `hold`, the
frames that render without stepping clear it explicitly, and a zone-in clears the mirrored copy — so
a stall can never survive into a zone the character has left, which matters more here than for
`hold` because a stall names an *anchor position* in the departed zone's coordinates. If the render
loop goes idle it stops recomputing, but a stalled body cannot be *freed* without a stepped frame
either, so idling cannot manufacture a stall; what it freezes is `secs`, detectable exactly the way
`held_secs` is (poll twice, compare the delta against your own clock).

### `zone_assets` — is the world this response describes actually loaded? (#579)

A zone's terrain arrives from the asset server as one large GLB (freportw: ~30 MB) and is decoded,
collided and uploaded on a background thread over **several seconds**. During that window the client
stands on a placeholder ground plane with **no collision at all**. Before this field existed the
client reported that as if it were the zone — a flat empty plain, an empty exit list, and a walker
that said `navigating` while steering a dead-straight line through geometry that had not been built.
That is exactly what produced the false #560 report ("flat plain, 0 collision, 700u unobstructed"),
which a later load on the same code refuted.

`GET /v1/observe/debug` therefore carries:

```jsonc
"zone_assets": {
  "state": "pending",            // "idle" | "pending" | "ready" | "failed" | "stale" | "unknown_zone"
  "reason": "zone_assets_pending",   // machine-readable why; null when ready
  "zone": "freportw",            // the zone the loaded/loading assets are FOR
  "player_zone": "freportw",     // the zone the client believes the character is in
  "status": "Downloading zone 3/7 (12.4 MB)…",   // live loader progress; failure reason when failed
  "terrain_meshes": null,        // mesh count, only when ready
  "collision_loaded": false,
  "detail": "…what this state means for anything the client says about the world…"
}
```

- **`ready`** is the only state in which the client's answers about zone geometry, exits, or
  navigability are about the real zone. It requires **both** that a terrain mesh count and a
  collision grid with geometry exist (`Ready` cannot be constructed without them) **and** that
  `zone == player_zone`.
- **`pending`** — keep polling. It is published on every zone change, in the same call that drops the
  previous zone's collision.
- **`failed`** is deliberately *not* folded into `pending`: the load is over and will not retry, so
  waiting for `ready` would hang forever. `status` says why. The client also declares a load failed
  if its loader thread panicked or its result was lost, so `pending` cannot persist with nothing
  behind it.
- **`stale`** — *the assets that are loaded belong to a different zone than the one the character is
  in.* `player.zone` is published by the network thread the instant `OP_NewZone` arrives, while the
  render thread starts the new zone's load on its next frame; in between (~66 ms, measured live) the
  previous zone's assets are still fully loaded. Answering then would describe the zone you just
  **left** — a wrong world, which is the same lie class as an empty one. Transient; poll on.
- **`unknown_zone`** — the client does not know which zone the character is in (before the first
  zone-in, or a zone-in that timed out — see `player.zone_in_failed`), so no assets can be matched
  to it.
- **`idle`** — no zone loaded and none loading.

> **The guarantee, and how it is verified.** *A `ready` observation is never about a zone the
> character is not in.* This is a universal, so it is held by a **property test**, not by a live run
> (a live run is an existence proof over one trajectory): `eqoxide_nav::zone_assets::usability` is
> the single decision function every consumer goes through, and
> `usable_iff_ready_for_the_zone_the_player_is_actually_in` asserts over the full cross product of
> state shapes × player-zone values that it returns "usable" **iff** the state is `Ready` and its
> zone equals the player's non-empty zone, while
> `no_interleaving_of_the_two_writers_yields_a_usable_wrong_zone` does the same across every
> interleaving of the two threads that write those values.

**Two endpoints refuse rather than answer while this is not `ready`,** with
`503 {"error": "zone_assets_not_ready", "reason": "…", "zone_assets": {…}}`:

| Endpoint | Why |
|---|---|
| `GET /v1/observe/zone_exits` | Exits come out of the collision grid; before it exists this returned a confident `[]` — "this zone has no exits at all" — and during `stale` it returned the *previous* zone's exits. |
| `GET /v1/observe/frame` | A PNG of the placeholder ground plane is indistinguishable from a genuinely empty zone, and a `stale` frame shows the zone you left. Pass **`?allow_pending=1`** if the loading screen is what you actually want. |

Every `200` from `/v1/observe/frame` also carries **`X-Zone-Assets-State:`** with the same word as
`zone_assets.state`, so a PNG fetched with `?allow_pending=1` cannot be mistaken downstream for one
of the real zone. Only `ready` means the image shows the zone the character is in. It also carries
**`X-Snapshot-Age-Ms`** (#646 — see [Per-endpoint freshness](#per-endpoint-freshness--snapshot_age_ms-646)):
a PNG body has no room for an in-band field, so the same freshness clock every other endpoint
carries rides this header instead.

**Endpoints that are deliberately NOT gated**, because they do not read zone geometry or collision
and are honest during a load: `/v1/observe/doors` and `/v1/observe/zone_entrances` (both are
server-pushed lists, not derived from the collision grid), and `/v1/move/manual` and `/v1/move/jump`
(they drive the controller directly and make no routing claim — though with no collision loaded the
character is moving through a world the client has not built, so prefer waiting for `ready`).

`POST /v1/move/goto` still accepts the goal, but its response carries a non-null
**`zone_assets_pending`** note while the assets are missing, and `nav_state` reads `zone_loading`
until they land.

### Camera override for `/observe/frame` (#422)

The live/persistent gameplay camera is often at an unhelpful angle for judging nav or collision
footing (e.g. staring up at the character's face) — exactly the moment a screenshot is most needed.
`GET /v1/observe/frame` accepts an **optional, STATELESS, per-request** camera override: it changes
only the one PNG this request returns and reverts immediately after — the on-screen/live camera
(and every capture after this one, with no params) is never touched. There is deliberately no
persistent "debug camera mode" toggle: a sticky flag is a new observable that can get stuck, so a
later plain capture could silently inherit a stale angle.

Pass **either** a named `preset`, **or** one or more of `pitch`/`yaw`/`distance` — mixing the two
is a `400`. No override params at all (or `?preset=default`) is byte-for-byte the pre-#422 behavior:
the already-rendered on-screen frame is read back, unchanged.

| Param | Meaning | Range |
|---|---|---|
| `preset` | A named diagnostic angle, relative to wherever the character is **currently facing** (so it stays correctly oriented no matter which way that is). One of `default` (no override), `top_down` (bird's-eye, ~85° pitch), `behind_above` (over-the-shoulder diagnostic view), `front` (facing the character head-on). | — |
| `pitch` | Elevation above the horizon, degrees. Positive looks down, negative looks up. Omitted → the live camera's current pitch. | `-85.0..=85.0` |
| `yaw` | Camera heading, degrees, same convention as `heading_ccw` on `/v1/observe/debug` (0 = north, increasing CCW). Unlike the presets, this is **absolute** — a fixed `yaw` always frames the same world direction regardless of the character's facing at capture time, so a scripted diagnostic angle is reproducible. Omitted → the live camera's current yaw. | `-360.0..=360.0` |
| `distance` | Camera distance from the character, world units. Omitted → the live camera's current distance. | `1.0..=2000.0` |

**Scoped to the four params in the table above** (this guarantee does not cover `allow_pending` —
see below): an invalid request against `preset`/`pitch`/`yaw`/`distance` — out-of-range value,
non-numeric value, unknown preset, `preset` combined with any of `pitch`/`yaw`/`distance`, or one
of these four **duplicated** (e.g. `?pitch=10&pitch=200`) — is always a
`400 {"error": "invalid_camera_override", "message": "…"}` that names the offending param — **never**
a `200` at a silently-clamped-or-ignored angle, and (since #701) never a non-JSON body either. The
duplicate-key case is checked by hand (`GET /frame` parses its own query string rather than using
axum's generic `Query` extractor) specifically so it lands in this same JSON shape instead of
axum's own plain-text `400` rejection, which is what it returned before #701.

**`allow_pending` is not a camera param** (it's the [zone-assets-readiness bypass
flag](#zone_assets--is-the-world-this-response-describes-actually-loaded-579)), so a duplicated
`allow_pending` (e.g. `?allow_pending=1&allow_pending=0`) does **not** get `invalid_camera_override`
— that would misname the subsystem the caller's mistake was actually in. It gets its own
`400 {"error": "invalid_query_param", "message": "…"}` instead (also JSON, since #701; before #701
this was axum's plain-text rejection like any other duplicate). An unrecognized query key —
duplicated or not, on either category of param — is still silently ignored either way, same as
before #701 (this endpoint has no `deny_unknown_fields` check, unlike `/observe/messages` and
`/observe/entities`).

An overridden capture is a separate, UI-free render pass (the on-screen frame — window chrome, HUD,
inventory, etc. — is built and presented first, completely unaffected either way): a plain
`GET /v1/observe/frame` shows exactly what's currently on screen, egui windows included, while any
`preset`/`pitch`/`yaw`/`distance` capture shows only the 3D world from the requested angle.

```
GET /v1/observe/frame                              # unchanged: today's on-screen frame
GET /v1/observe/frame?preset=top_down               # bird's-eye view, wherever the character is facing
GET /v1/observe/frame?preset=behind_above            # over-the-shoulder diagnostic view
GET /v1/observe/frame?pitch=30&yaw=90&distance=150   # explicit angle, independent of facing
```

### `matched` — which entity a name actually resolved to (#513)

Name-resolving endpoints — `POST /v1/move/goto {name}`, `POST /v1/move/follow {name}`, and
`POST /v1/combat/target/name` — return a **`matched`** object naming the entity they actually
picked. **Check it.** A name is fuzzy-matched against the live spawn table, and before #513 these
endpoints returned only coordinates / a bare success, so a resolution that silently landed on a
*different* spawn than you meant was undetectable (a live near-miss routed `"a_rodent020"` to a
distant NPC named `Astaed_Wemor`).

```jsonc
POST /v1/move/goto {"name": "a gnoll"}
{
  "status": "navigating",
  "goal": [-41.1, 3157.1, -3.1],
  "goal_id": 12,
  "matched": {
    "id": 437,           // spawn id actually routed to / targeted
    "name": "a gnoll",   // its canonical (cleaned) name
    "quality": "exact",  // "exact" | "fuzzy"
    "candidates": 5,     // how many spawns matched EQUALLY well
    "distance": 1163.2   // units from you; OMITTED when not known
  },
  "note": "..."
}
```

| Field | Meaning / how to gate on it |
|-------|------------------------------|
| `id`, `name` | The entity actually routed to / targeted. Guaranteed to describe the same spawn as `goal` — they are derived from one value and cannot drift apart. |
| `quality` | `"exact"` = a case-insensitive match on the full name. `"fuzzy"` = **no exact match existed**; this is only a substring hit, so verify it before acting. An exact match is never passed over for a nearer fuzzy one. |
| `candidates` | How many spawns matched at that same quality. `1` = unambiguous. **`> 1` means the name was ambiguous** (e.g. 17 spawns all named "a large field rat"); the **nearest** of them was chosen. Gate on this if you need a specific spawn — target by `id` instead. |
| `distance` | Units from the player. **Omitted** — not zero — when the entity has no known position or the server has not told us our own yet (just after zone-in). Never a figure measured from the zone origin. |

`matched` is `null` only for a `/goto` given raw coordinates (there is no entity). A name that
matches nothing at all — not even fuzzily — is an honest **404**, never a distant wrong match.

> **Content type:** `/v1/move/goto`, `/v1/move/follow` and `/v1/combat/target/name` return
> `application/json`. The other `move` routes (`/stop`, `/zone_cross`, `/manual`, `/jump`) still
> return `text/plain`.

### `nav_goal_id` and `nav_goal` — goal identity (#349)

`GET /v1/observe/debug` carries two more fields. **They are top-level — siblings of `player`, not
inside it**, unlike `nav_state` and `nav_reason`, which are inside `player`. (Measured on a live
client while checking the rule below; the previous wording said "top-level fields under `player`",
which is not a place, and an agent that took it literally would read `null` forever.)

- **`nav_goal_id`** — a monotonically increasing counter, bumped every time a `POST /v1/move/{goto,follow,zone_cross,stop}` is accepted. It is **echoed in each of those POST's response bodies**: as a JSON `"goal_id": N` field on `/goto` and `/follow`, and as `[goal_id=N]` in the text body of `/stop` and `/zone_cross`. `nav_state`/`nav_reason` are the status *of this goal id* — never of an earlier one. **`/zone_cross` is the one route whose returned id does not end up carrying the outcome**: resolving the request into a concrete walk stamps a fresh, higher id (see below).
- **`nav_goal`** — that goal's `[x, y, z]` (server coords), or `null` for `idle`/`stop`, or for a `zone_cross` whose concrete zone-line destination the walker has not resolved yet. **`nav_goal` is `null` on every `idle`, whichever `nav_reason` got you there** — `zoned`, `goal_dropped`, `respawned`, `stopped`, `goto_superseded`, `zone_cross_dropped_unhandled` — because the coordinates are a fact about the goal and the goal is over (#732). The `zoned` case is the sharp one and the one that was actually wrong: coordinates are a **per-zone namespace** and carry no zone tag, so a goal that survived a crossing was a well-formed, numerically-plausible answer about the zone you just left. `nav_goal_id` deliberately does **not** reset — it is identity, not a per-goal fact, and it is what lets you match the `idle` to the request that produced it.

**Why this exists.** `POST /goto` returns `200` and sets the target, but the walker only re-labels `nav_state` on its next ~150 ms tick. Without identity, this canonical loop lied:

```
POST /v1/move/goto {...}   -> 200 {"goal_id": 8, ...}
GET  /v1/observe/debug     -> nav_state: "arrived"   <-- but nav_goal_id: 7, the PREVIOUS goto!
```

Now the accept **atomically** bumps `nav_goal_id` and resets `nav_state` to `pending`, so the read above returns `nav_state: "pending", nav_goal_id: 8` — honest.

**Rule: ignore any `nav_state` whose `nav_goal_id` is LOWER than the `goal_id` your POST returned — that is an older goal's outcome. At your id or above, the state is current.** A matching id with `pending`/`planning`/`navigating` means your goal is genuinely in flight — and it will not stay that way, because any in-progress state with nothing behind it retires to `idle` with a reason on the next walker tick ([Why an in-progress `nav_state` can never stick](#why-an-in-progress-nav_state-can-never-stick-725)). `idle` **at or above your own goal id** is therefore an outcome, not a "not started yet": read `nav_reason`, which since #725 always says which outcome it was.

**Why the rule is "at or above" and not "equal" — `/v1/move/zone_cross` in particular.** A *higher*
id does not mean your read is stale; it means your goal was superseded, **or that the client
advanced your own request to its next stage under a fresh id.** `/zone_cross` always does the
latter. The id its 200 returns identifies the *request*; the request has no coordinates yet, so
`nav_goal` is `null`. One walker tick later the client resolves the requested zone to a concrete
zone-line region and issues the walk to it internally, and that walk stamps its own new, higher id —
under which every subsequent `navigating`/`arrived`/`no_path` for your crossing is published. **The
id from the `/zone_cross` 200 therefore never carries the outcome; waiting for an exact match waits
forever.** (On every crossing measured so far — seven — the resolved id was the accepted id
+ 1, but do not key on `+1`: any concurrent accept from another caller shifts it. `>=` is the
property that holds.)

The reliable way to follow a crossing to completion is not the id at all: poll `player.zone` (and
`crossing_pending_ms`), and treat `nav_state: "idle"` with `nav_reason: "zoned"` as the success
signal — see [Every `idle` says how it got there](#every-idle-says-how-it-got-there-725). Use the
returned id only for its guarantee: any state below it is not about you.

**`goal_z_snapped` — the client CHANGED your goal.** The `z` you gave sits below every floor in the
goal's column (agents commonly pass `z: 0`, or a map coordinate), so the planner snapped the goal onto
the real floor at that XY and routed there. You are being walked somewhere you did not literally ask
for, so you are told — on `navigating` **and on `arrived`**, plus a line in the message log. If the z
matters to you, re-issue with the real floor height. (A goal with **no** floor anywhere in its column
is not snapped: it fails as `no_path` / `goal_not_walkable`.)

`nav_reason` for `no_path`:

| Reason | Meaning |
|--------|---------|
| `goal_not_walkable` | The goal has no walkable floor under or near it — it's inside geometry, off the mesh, or floating in the air. **Fix your goal's coordinates.** Reported immediately, without searching. |
| `search_closed` | The planner explored every cell reachable from the character and the goal was not among them. Genuinely walled off. |
| `start_isolated` | The *character* is boxed in (inside a tree trunk / on a slope face), and re-anchoring to nearby floor didn't help. |
| `no_geometry` | No collision mesh loaded yet (still zoning). |
| `planner_dead` | The pathfinding worker thread has **died**. No route can be planned for the rest of the session — a **client fault**, not an unreachable goal. Movement must be driven manually, or the client restarted. This is reported loudly and terminally rather than leaving `nav_state` stuck at `planning` forever. |

`POST /v1/move/zone_cross` reports two further `no_path` reasons, both specific to zone-line crossing (#267):

| Reason | Meaning |
|--------|---------|
| `no_zone_line_to_zone` | The server never advertised (`OP_SendZonepoints`) any zone line from here to the requested `zone_id` — it will not appear in `/v1/observe/zone_exits` either. A genuinely invalid request: pick a `zone_id` that's actually one of this zone's exits. |
| `zone_line_not_in_map` | The requested `zone_id` **is** advertised by the server as a real exit, but the locally loaded zone geometry has no matching WLD zone-line (DRNTP) trigger region for it — a client-side `.wtr` map-data gap, not proof the exit doesn't exist in the real game. Before reporting this, the client tries one fallback (#683): if the map has a zone-line region under some OTHER (unadvertised) index — e.g. an exit baked with index 0 — and this zone advertises no same-zone teleport pad and server zone points are available, it walks there instead and lets the server resolve the destination, so this reason now means no usable zone-line region exists at all (or the fallback is gated: a same-zone pad is advertised, or no server zone points are available). When the fallback **is** taken you do not have to infer it from the message log: `zone_cross_best_effort` on `/v1/observe/debug` says so structurally (#713, [below](#zone-cross-degradations-you-can-detect-713)). It is also omitted from `/v1/observe/zone_exits` (which only lists regions actually found in the loaded map), so "absent from `zone_exits`" does not by itself distinguish this from `no_zone_line_to_zone` — only `nav_reason` does. |

**Distance no longer decides *whether* the client acts (#725).** It still decides how long the
crossing takes — you are walked to the line, so a line 30 u away takes longer than one 3 u away —
but there is no distance at which the request is handled differently, and none at which it is
handled by doing nothing. When a zone-line region *is* located, the client always walks to it and
re-stamps `nav_goal` with that concrete destination; there is no "close enough, the crossing will
happen by itself" shortcut. There used to be one, for lines within 15 u,
and because the actual auto-cross fires only when your body is physically inside the trigger region
(a much smaller volume), every `/zone_cross` issued from between the region and 15 u was consumed and
produced nothing at all. That band is where the server's own walk-in arrival point tends to sit
relative to the *return* line, so "walk into a zone, then ask to go back" was the case most likely to
hit it.

### Zone-cross degradations you can *detect* (#713)

Two zone-cross conditions used to be reported only as English in the message log. Prose in a log is
not a machine-readable signal, so each now has a structured disclosure on
`GET /v1/observe/debug`, in the same shape as the other top-level disclosure objects there: an
object with a `reason` token and a human `detail`, or **`null` while there is nothing to disclose**.

| Field | Non-null means |
|-------|----------------|
| `zone_cross_best_effort` | Your last `/v1/move/zone_cross` **degraded to the #683 fallback**: the client located a zone-line region but nothing tells it where that line goes, so it is walking you there and letting the server pick the destination. `{"reason":"server_resolved_destination","requested_zone_id":N\|null,"region_index":I,"detail":…}`. It may land you somewhere other than the zone you asked for — check `player.zone` after `nav_state` reaches `zoned`. Set at resolution time; **cleared at the top of every subsequent `/zone_cross`**, including one that resolves to no line at all, and on zoning. Those are the *only* clearers: **nothing retires it when the walk arrives, is blocked, or is stopped**, so it describes the most recent resolution in this zone and **not necessarily one still in flight** — it can be non-null at the same time as `zone_cross_stopped`. Read `nav_state` to find out whether a crossing is actually in progress. |
| `zone_cross_stopped` | The automatic crossing that fires while you stand on zone-line geometry has **given up**: `{"reason":"cross_attempt_limit","region_index":I,"attempts":3,"detail":…}`. Nothing further will be attempted for as long as you stand there. |

**The attempt bound.** Standing on a zone line makes the client attempt a crossing every ~10 s
(the auto-cross cooldown). If the server denies it — or simply never answers — that used to repeat
**forever**, one server-side anomaly per attempt. The client now makes at most **3** attempts during
one continuous stand and then stops.

Three is a judgement, not a measurement: 1 is too few because a denial can be transient, the
interval is the 10 s cooldown so 3 spans ~20 s of retrying, and an over-tight bound is cheap to
recover from while an over-loose one is not.

The terminal state is deliberately **observable and distinguishable** — a bound that replaced an
infinite retry with silence would be worse than the retry, because you would wait forever for a
crossing that will never be attempted again. When the bound is reached the client (a) emits one
message-log line naming the field to look at, and (b) publishes `zone_cross_stopped`. Note what it
does **not** claim: the client cannot tell a refusing server from a silent one, because
`OP_ZoneChange` carries no correlation id, so `attempts` counts *attempts made*, not *denials
received*, and `detail` says so.

**Re-arming.** The tally is cleared when the client observes you standing on **no** zone-line region
— i.e. step off every zone line and back on. Two consequences worth planning around:

- The tally is **per stand, not per region**. If a stand spans more than one region (you shuffle
  between two adjacent zone lines), the three attempts are shared across them, so a region that
  would have crossed can be refused because a *different* region in the same stand used up the
  budget. This is the accepted cost of a bound that a walk-off/walk-on oscillation cannot defeat.
- Clearing happens on the **first tick** the client observes you off every zone-line region (the
  net-thread tick, ~100 Hz) — not on the 10 s auto-cross cooldown. *(An earlier revision of this
  bullet said the opposite, and it was worse than stale: the reset used to sit inside the cooldown
  guard, and because the bound-reaching attempt stamps that cooldown itself, a walk-off/walk-on
  shorter than 10 s cleared nothing **at all**, not merely late. Fixed and measured in #713 review
  round 2, B2 — `the_escape_hatch_works_on_the_tick_you_step_off_713`.)*
- The **crossing** is still rate-limited to one attempt per ~10 s cooldown, so stepping back onto the
  line re-arms the allowance but does not fire an immediate retry.
- **A fresh `POST /v1/move/zone_cross` does not re-arm the bound.** Only stepping off every
  zone-line region, and zoning, clear the tally. An explicit retry on the same line returns `200`,
  walks nowhere (you are already there) and never crosses; `zone_cross_stopped` stays non-null
  throughout, so it is detectable — but it is the first thing to try and it is not a recovery.
  *(Reasoned from the code path, not measured live: `resolve_zone_cross` has zero occurrences of
  `zone_cross_attempts` in its body, so it cannot be the tally's fourth writer — but no test drives
  an explicit retry end-to-end and asserts the HTTP response or the walk outcome.)*

`nav_reason` for `blocked`:

| Reason | Meaning |
|--------|---------|
| `walker_stalled` | The fine planner *can* thread the route from here, and the walker still didn't move: a genuine collision/steering wedge. `POST /v1/move/manual` (optionally `"jump": true`) may free it; then re-issue the `goto`. |
| `local_no_way_through` | The **fine 2u planner closed its whole 40u window** without finding a way along the committed route, OR the walker spent its proactive-re-plan budget re-routing the same impasse without progress (the qcat L-corner class). The corridor is not threadable at the character's own collision radius — this is *not* a slide/collision wedge, and nudging will not fix it. A coarse route to the goal may exist, but the walker cannot follow it around this corner. Approach the goal from another direction. (#382, #378) |
| `fall_would_be_lethal` | The next waypoint is down a drop whose fall damage would likely kill the character. Stopped at the ledge. |
| `no_progress` | The walker kept moving but its **closest approach to the goal did not improve for 60 s** — a lap/eddy a route follows without ever getting closer (e.g. swimming a moat ring), which the `walker_stalled` detector (it only watches for a walker that *stops*) misses. Not a physical wedge: a coarse route keeps being followed, it just does not close on the goal. Approach from another direction or pick a reachable waypoint. Measured as 3‑D closest approach, so a spiral ramp / vertical climb is never mistaken for no progress, and only fires for a **fixed-destination** goto (never a `/follow` chase). (#631) |

### `nav_blocked_by` and `nav_tier` — the blockage payload (#378)

`GET /v1/observe/debug` carries two more top-level fields whenever navigation has something to say:

- **`nav_blocked_by`** — behind a terminal `no_path`, WHAT is blocking and WHERE. `null` when there is nothing to report (not a terminal `no_path`, or the diagnosis could not be computed — honest silence, never a fabricated hazard). Shape:
  ```json
  "nav_blocked_by": {
    "goal":     { "hazard": "floor", "at": [x, y, z] },   // or null
    "frontier": { "hazard": "wall",  "at": [x, y, z] }     // or null
  }
  ```
  `goal` is **definitive** — the goal itself cannot be stood at (pairs with `goal_not_walkable`); if it is present, no search could ever have succeeded. `frontier` is the hazard at the search's **closest approach** to the goal (pairs with `search_closed`, the common sealed-corridor wedge where the goal is fine but you are walled off from it). `hazard` is `floor` | `wall` | `water`. **`frontier` is ONE blocking fact — not necessarily the only one, and not necessarily the one to fix.** It is computed only on a FAILED plan (never on a successful one), and only when even the character's own collision radius does not fit, so it never over-claims a wall the walker could have squeezed past. Computed by the same `Traversability` authority the planner uses, so it cannot disagree with what the planner actually refused.
- **`nav_tier`** — which clearance tier the CURRENT route was found at: `"minimum"` (threaded a tight gap at the character's own collision radius — riskier, no margin from walls/drops), `"preferred"` (the roomy tier carried it), or `null` (no route committed). This is the **per-route** fact for the route being walked right now — distinct from the zone-lifetime `nav_tight` counter, which aggregates over the whole zone and cannot answer "is *my* route tight?".

---

## `nav_declined_pads` — the teleport pads nav refused, offered back to you (#543 / #266)

Some zones advertise **teleport pads**: DRNTP regions you walk onto and get repositioned. When a
pad's advertised `target_zone_id` equals the zone you are standing in, it *looks* like an intra-zone
shortcut — and the planner used to route through it.

**It cannot be trusted, and nav will not route you through one.** The advertised `zone_id` is one
zone-point row's target, but the server resolves an organic crossing by an index-blind, nearest-XY
match over **every** row's *trigger* coordinates — data the wire never carries. So a pad advertised
as same-zone can resolve server-side to a **different zone**, and in North Qeynos it does exactly
that: a `/goto` across such a pad silently landed the character in another zone entirely. **There is
no such thing as a *verified* same-zone pad here.** A goal reachable only across one is therefore an
honest `no_path`.

But a bare `no_path` next to a perfectly real pad would be its own quiet falsehood, so
`GET /v1/observe/debug` **discloses** what nav declined, in top-level `nav_declined_pads` (a sibling of
`player`, not a field inside it). `null` unless nav is in a terminal
no-route state (`no_path` / `search_exhausted`) **and** it declined at least one pad:

```json
"nav_declined_pads": {
  "reason": "advertised_same_zone_unverifiable",
  "pads": [
    {
      "index": 2,
      "footprint": [-611.2, -76.3, -14.0],
      "footprint_count": 58,
      "alternates": [[-606.1, -69.5, -14.0], [-729.1, -70.2, -14.0]],
      "region_at": [-611.2, -76.3, -14.0],
      "advertised_dest": [-153.0, -30.0, 9.0],
      "advertised_dest_floor": [-153.0, -30.0, 6.0],
      "advertised_same_zone": true,
      "destination_verified": false
    }
  ],
  "detail": "..."
}
```

- **`footprint`** — the spot to **try** (`/v1/move/goto`): the standable point inside the pad's trigger
  region nearest you, measured in this client's own collision mesh and re-picked as you move.
  **A candidate, not a promise** — verified live that walking to one spot on a pad fired nothing while
  another spot on the *same* pad crossed immediately, and a `goto` stops within its arrival tolerance,
  which can leave you just outside a small trigger. `null` means no standable spot was found in the
  region at all; walking to `region_at` may then do nothing. Either way it is a warning, not a
  disqualification — the region is really there, and the standability probe is this client's model.
- **`footprint_count`** / **`alternates`** — how many standable spots this pad has in total, and up to
  7 more to try if the first fires nothing. Nearest-first, and **thinned so each is a genuinely
  different place** (at least one nav cell apart): a pad's region is a BSP, so its nearest leaves are
  often many names for the same spot — one observed pair was 0.0005 units apart. A DRNTP region is a BSP and one
  pad routinely has dozens of spots (58 for the North Qeynos pad above), so you get **one offer per
  pad**, not one per spot. **If nothing happens, work through `alternates` before concluding the pad
  is inert.**
- **`region_at`** — where the region itself is, present even when nothing in it is standable, so a pad
  is never reduced to "somewhere in this zone".
- **`advertised_dest`** — the server's **advertisement, verbatim off the wire** (wire z datum). **Not**
  where the pad goes. `null` means the pad advertises no arrival at all (the keep-position sentinel) —
  which does **not** make it un-takeable, you simply have no claim to compare your observation against.
  There is deliberately no unqualified `dest` key.
- **`advertised_dest_floor`** — where that advertisement lands on **this client's** floor model, or
  `null` if it found no floor in that column. This is a client derivation, reported separately so it can
  never be mistaken for the server's claim. **`null` here is not a reason to skip the pad** — it is a
  fact about the advertisement, and the advertisement is the untrustworthy part.
- **`destination_verified`** — always `false`, in machine-readable form. Nothing the client can observe
  from the wire ever makes it `true`.

**The client does not remember where a pad landed.** That memory is yours. If you take one, read
`player.zone` and `player.pos` afterwards to find out where it actually went — that observation is
the only thing that establishes a pad's real destination, and only you keep it.

> ⚠️ **`player.pos` / `player.zone` are PROVISIONAL for a moment right after any crossing — and
> `player.position_provisional` tells you exactly when.** To make the character leave the pad's trigger
> region, the client applies the *advertised* arrival to its own position immediately, before the
> server has said anything. The zone echo then settles **which zone** (and `zone` flips there) while
> the position does not arrive until the new zone's first update — so in that window `zone` and `pos`
> can genuinely disagree. Do not read them as settled until:
>
> ```
> "position_provisional": false,   // true while pos/zone are the client's own guess
> "crossing_pending_ms": null      // ms it has been unsettled (measured at read time)
> ```
>
> Both are under `player` on `GET /v1/observe/debug`. `position_provisional` clears only when the
> **server** says where you are — never on the zone echo alone. The message log says the same thing at
> the moment of the crossing, but **do not rely on it**: it is a ring buffer and ambient chatter can
> evict the line within seconds. The field is the observable.

**A pad is offered whenever it exists in this client's loaded map.** That is the only bar, and it is
answered from geometry the client measured — never from the advertised destination, which is the part
it cannot trust. `advertised_unusable` means something narrower than it sounds: the server advertised
an index this client's map has **no region for at all** (a `.wtr` data gap), so there is nothing to
point you at. The full per-pad record (including the `unknown` and `advertised_unusable` states, which
carry no offer) is on `GET /v1/observe/nav_debug` under `pads`, keyed by `knowledge`.

**Nav declines to route through these on its own initiative; it does not stop you.** `POST
/v1/move/zone_cross` and walking onto a footprint yourself both still work — that is the point of
disclosing them.

---

## The fine steering tier (`nav_local`) — #382

Navigation has two tiers. The **coarse** one (8 u cells, whole zone) chooses the route and produces
`nav_state`. The **fine** one (2 u cells, a 40 u window, re-planned every nav tick) is what actually
**steers** the character along the last few strides of that route — threading the thin ramps and narrow
openings the 8 u grid cannot see. `GET /v1/observe/debug` carries **`nav_local`** (top-level, not under `player`): what that tier last
said. It is **`null` while the tier is healthy** (a complete fine route to its carrot), exactly like
`nav_support` / `nav_tight`.

```json
"nav_local": {
  "state": "no_way_through",
  "reason": "search_closed",
  "stuck_ticks": 2,
  "plan_us": 14300,
  "detail": "..."
}
```

| `state` | Meaning |
|---------|---------|
| `no_way_through` | The fine planner **closed its entire 40 u window** and found no way along the committed coarse route from here. A falsifiable **local** "no" — the coarse route is being re-planned around it. It says **nothing** about whether your goal is reachable. |
| `exhausted` | The fine search was **cut short** (node cap) before closing its window: **"I don't know"**, not "no". The walker is steering on the best partial it has. |
| `planner_dead` | The fine worker thread has **died**. Steering has degraded to the coarse 8 u route for the rest of the session — the character **keeps walking**, but handles thin ramps and narrow openings worse. A client fault; restart to recover it. **Do not poll for this here** — it is a session fault, not a per-goal verdict, so it disappears from `nav_local` when the goal is retired. Read `nav_local_planner_dead` instead. |

> **`nav_local.state` is never `no_path`, and structurally cannot be.** A 40 u window can never prove a
> *goal* unreachable, so a tight doorway must never be able to tell an agent its destination does not
> exist. Only the coarse planner, which closes the whole zone's frontier, may say `no_path`.

**`nav_local` is `null` on every `idle`, whichever `nav_reason` got you there** (#766) — `zoned`,
`goal_dropped`, `respawned`, `stopped`, `goto_superseded`, `zone_cross_dropped_unhandled`. The fine
tier's verdict is about threading toward *a goal*, so when the goal is retired the verdict goes with
it, exactly like `nav_goal` and `nav_tier` (#732). The `zoned` case is the sharp one: a `no_way_through`
left standing across a crossing describes a corridor in the zone you just left, computed against a
collision grid that no longer exists — and it is the only kind that publishes, because a healthy
`threaded` verdict is filtered out anyway. It **does** survive a terminal `blocked` / `no_path`, on
purpose: there it is the *evidence* behind the failure you are being told about.

This holds for the whole `idle` row, not just the moment it becomes `idle`, and it takes two writers to
say so. `NavStatus::retire_to_idle` clears the field on the transition; `Walker::set_nav_local` then
refuses to store a verdict onto a row that is already `idle`, which is what covers a fine-tier reply
that was still in flight on another thread when the goal was retired.

**Why this field exists.** The fine tier is bounded *spatially* (a 40 u window) plus a deterministic
node cap (#394 removed its old 150 ms wall clock, so its answer no longer depends on machine load), and
until #382 it ran **inline on the network thread**, every nav tick — the last A* left on that thread, a
residual stall of the class that caused the #257/#302 linkdead drops (measured, release/akanon: mean
**15.3 ms**, worst **358 ms**). #382 moves it onto its own worker thread: the walker keeps steering on
the last good fine plan while a new one computes, so nothing real-time waits on the fine search. That
move is also where `nav_local` comes from — the honest `LocalOutcome` (`threaded` / `no_way_through` /
`exhausted`) the worker reports, so an agent watching a character grind at a doorway can tell "the
corridor is not threadable" from "the steering planner hasn't caught up." `nav_local` is where you read it.

> **The distinction between `no_path` and `search_exhausted` is load-bearing, and it is new (#337).**
> They used to be the same thing — worse, an unreachable goal didn't report *either*. The planner
> handed the walker a greedy partial route, the walker drove it into a wall, retried 8 times, and
> froze at `blocked` forever, never once saying "there is no way there". That silent wedge disguised
> the real nav root cause for months and caused several false diagnoses. **A timeout is never
> reported as "no route"**, and an unreachable goal is now reported before the character takes a
> single step.

### `nav_local_planner_dead` — fine-planner liveness, session-scoped

```json
"nav_local_planner_dead": false
```

**Always present, in both states.** `true` once the fine worker thread has been observed dead. It
stays `true` for the life of that worker, because the thread does not come back — recovering it needs
a client restart.

*Session-scoped* is the agent-facing name for that lifetime, and it is accurate: the latch the client
keeps internally is scoped to the fine **worker**, and exactly one fine worker is built per client
process, so from out here the two are the same span. The distinction only matters to the client's own
code, which is where it is written down — as is the fact that nothing currently pins the one-worker
premise this name rests on (#787).

**Over a running client this field is one-way.** Nothing in the process constructs a second fine
worker, so nothing clears it. That is a property of the *process*, not of the field, and it was
equally true before the clear described next existed.

**A `false` reading is only meaningful while `net_thread_dead` is `null`.** The client clears this
flag in one place — when it constructs a new fine worker — and nothing writes it when a worker *ends*
without a replacement. Once the network thread has exited, the fine worker is gone and this row, like
the rest of the payload, is frozen at whatever it last held; `net_thread_dead` is the field that tells
you so. Check it before you trust a `false` here.

> The clear-on-construction exists so that if anything ever *did* build a second fine worker
> in-process, the new one would not inherit the old one's latch and report a fault the client had just
> repaired. On today's single-worker process it is a no-op.

This field exists because the `nav_local`-is-`null`-on-`idle` rule above would otherwise hide a client
fault. Two of `nav_local`'s three states — `no_way_through`, `exhausted` — are verdicts about
threading toward *a goal*, so retiring them with the goal is right. `planner_dead` is not: it is a
latched fault about the client itself that happened to be riding in the same field. Left there, it
would vanish on every retirement, so an agent **between goals** — exactly when you poll
`/v1/observe/debug` to decide what to do next — could not learn that its steering had degraded to
the coarse 8 u route with nothing on any nav route to recover it. It would reappear only after a new
goal had committed a route.

So check liveness here, not in `nav_local`. `nav_local` still reports `planner_dead` while a route is
committed, which is useful in context; this is the channel that does not vanish when the route does.

**One honest limit.** The worker's death is only *discoverable* through a failed send or a
disconnected receive, and both happen on a tick that has a committed route. A worker that dies and is
never posted to again is not detectable by any reader, this field included. What is guaranteed is that
once the death has been seen, it stays visible for the rest of the session.

---

## Connection health

`GET /v1/observe/debug` carries eleven fields that tell you **whether the rest of the payload can be
trusted at all**. Six are about what the SERVER did (below); five are about what the client itself
failed to send (see [Outbound send failures](#outbound-send-failures)). They are computed when you ask — not cached — so nothing has to be running inside
the client for them to be right (#343).

| Field | Meaning |
|-------|---------|
| `connected` | **Is the link up?** `false` after 15s with no inbound datagram of any kind. Use this for "am I disconnected?" — but it only proves the SOCKET ACKs, not that the world is alive (see `world_responsive`). |
| `link_age_ms` | ms since any inbound UDP datagram, session-layer ACKs included. `connected` is derived from this. |
| `last_packet_age_ms` | ms since the last *world update* (an application packet). |
| `snapshot_age_ms` | ms since the client's network thread last ticked. |
| `world_responsive` | **Is the WORLD alive, not just the socket?** `false` only when an active liveness probe went unanswered past its bound while the link kept ACKing — a wedged zone. `true` for a healthy zone, including a legitimately idle one (the probe is answered). `true` before the first probe fires. See below. |
| `last_world_response_ms` | ms since the world last *proved* it processed something for us — a probe reply or a spontaneous packet, whichever is fresher. The companion to `world_responsive`. |
| `send_failures` | **Datagrams this client BUILT but could not put on the wire** — the datagram never reached the wire and **nothing will re-send it**. Covers more than a kernel refusal: non-transient errors (`EMSGSIZE`, a dead socket), queue-overflow evictions, and datagrams still queued when a session ends all land here. Cumulative since process start. **`0` is the expected healthy reading** (since #641 — see below). |
| `send_wouldblock_rescued` | Datagrams whose `try_send` returned `WouldBlock` and which an **immediate direct `send(2)` on the same fd then accepted**. They reached the wire, so they are not failures. Cumulative. An **upper bound** on tokio's synthetic-`WouldBlock` case — see below for why it is a bound and not a measurement (#641). |
| `send_deferred` | Count of **datagrams** (not refusal events) that a transient send refusal caused to be QUEUED for retry on a later ~10ms tick, rather than dropped. Only session-layer control (ACKs, keepalives, session setup) is deferrable. A **lower bound** on genuine kernel refusals. **Not disjoint from `send_failures`** — see below (#641). |
| `send_starved` | **`true` only while a send-pressure burst is ONGOING right now; `false` the instant it ends — the ALERT `send_wouldblock_rescued`/`send_deferred` themselves cannot give you (#656).** Both of those counters are cumulative since process start and can only grow, so neither can answer "is the io driver starved *right now*", only "has it ever been". `send_starved` can — see below for the fire/clear rule. |
| `send_failures_unretried` | The subset of `send_failures` with no client-side retransmit of that datagram. |
| `last_send_error` | `ErrorKind` name of the most recent send failure (`"WouldBlock"`, `"Uncategorized"`, …), or `null`. |
| `last_send_error_age_ms` | ms since that failure, measured at read time, or `null`. Distinguishes an old blip from an ongoing failure. |
| `reliable_abandoned` | **Un-ACKed reliable datagrams left outstanding when a session ENDED** — the loss `send_failures_unretried` cannot see. Cumulative. Measured `0` across three clean zone handoffs, so **a nonzero value during play is signal** (clean shutdown is the measured exception). Does not cover a server-side session drop — see below. |

**`last_packet_age_ms` is not a disconnect signal.** An idle EQ session — a character sitting alone
in a quiet zone — routinely goes **40+ seconds with no application packet** while the link is
perfectly healthy (the server only pushes HP/mana/position on *change*). Treating a high
`last_packet_age_ms` as a dead connection will send an agent into a pointless reconnect loop. Read it
as *"the world is quiet"*, and use `connected` to decide whether the link is gone.

> **Changed in #343.** `connected` previously derived from application traffic and was recomputed
> only when a frame rendered — so a dead connection (no packets → no render) reported
> `connected: true`, frozen, forever. It now derives from link liveness, at read time.

### Outbound send failures

Every other health field is about what the *server* did. These four are about what *we* failed to
do. Until #612 the client's send path ended in `let _ = self.socket.try_send(&raw)` — **every** send
error, for **every** packet it ever transmitted (`WouldBlock`, `ENOBUFS`, `EMSGSIZE`, `ENETUNREACH`,
a dead socket), was discarded. A datagram that never left the machine was therefore
indistinguishable from one the server received, and an agent issuing a command had no way, even in
principle, to learn that the command had not gone out.

Every send now funnels through one place that records its own failure, so:

- **`send_failures: 0` IS the expected healthy reading, and a nonzero value means a send was
  refused and not recovered.** This bullet used to say the opposite; the reversal is #641. The
  measurement that prompted it: a fresh, healthy login into `qeynos` read **`send_failures: 283`** —
  all `WouldBlock`, all 7-byte session-layer control datagrams (ACKs), in a burst during zone-in and
  then flat. Those ACKs never reached the wire, so the server kept retransmitting datagrams it had
  not seen acknowledged. The client now (a) re-attempts any `WouldBlock` datagram immediately via a
  direct `send(2)`, and (b) queues a transiently-refused *control* datagram and re-sends it on the
  next tick. Both outcomes are counted separately from `send_failures`, which is again reserved for
  "this datagram never reached the wire and nothing will re-send it".
- **What triggers it: CPU starvation of the client's io driver.** Pinning the whole client to one
  core reproduces a burst on roughly 1 login in 6, on two different machines and two different zones;
  an unloaded client reads 0. That is the reproducible part.
- **What the two new counters do NOT tell you: which mechanism refused the send.** Two mechanisms
  can produce the same `WouldBlock` from `try_send`:
  1. tokio short-circuits on an empty cached readiness bit and returns a **synthetic** `WouldBlock`
     *without issuing the syscall at all*; or
  2. the readiness bit is set, the syscall IS issued, and the **kernel** returns `EAGAIN`/`ENOBUFS`
     (which also clears the bit).
  A direct `send(2)` that succeeds microseconds later is consistent with (1) — but equally with (2)
  followed by the transmit buffer draining in between. So `send_wouldblock_rescued` is an **upper
  bound** on (1) and `send_deferred` a **lower bound** on (2); neither is a measurement of either.
  A double refusal (both the `try_send` and the direct `send(2)`) *is* hard evidence of (2).
  Distinguishing them properly would need something like `ioctl(SIOCOUTQ)` on the fd at the moment of
  the refusal (≈0 queued bytes ⇒ genuinely synthetic); that has not been done.
- **Both mechanisms occur, and the split varies RUN TO RUN — not by zone, not by machine.**
  Instrumented single-core-pinned zone-ins: `qeynos` **141 rescued / 107 refused-again**, then
  **166/106** and **119/114** on later runs; `gfaydark` **0 rescued / 138 deferred** on one run and
  **175/147** on another — *same zone, same recipe, same binary*. An earlier draft of this page
  attributed that `0` to the zone; the second `gfaydark` run refutes that, and the real conclusion is
  stronger: you cannot predict the split from anything observable, so do not try. What IS
  established is that "it is all synthetic" is FALSE (the double refusals prove it), and that the fix
  is agnostic — it recovers both.
- **`send_wouldblock_rescued` and `send_deferred` are load signals, not loss signals.** Every
  datagram counted by `send_wouldblock_rescued` reached the wire; every datagram counted by
  `send_deferred` was queued and, in the normal case, went out on a later tick. Both climb under CPU
  pressure and are `0` on an unloaded client.
- **`send_deferred` is NOT disjoint from `send_failures`, and must not be read as "all of these were
  delivered".** A deferred datagram is counted once, when it is queued. If it is *later* lost — the
  queue overflows (bounded at 1024; the oldest is evicted, since `OP_ACK` is cumulative), or the
  session ends while it is still queued — that loss is counted in `send_failures` /
  `send_failures_unretried` as well, so the same datagram appears in both. `send_failures` remains
  the honest "was anything lost?" number; `send_deferred` answers "how many datagrams did the socket
  make us delay?".
- **`send_starved` is the alert #656 added because nothing consumed the two counters above it.**
  PR #653 added `send_wouldblock_rescued`/`send_deferred` to this endpoint, but a plain
  cumulative-since-start counter can only grow — an agent polling it has no honest way to tell
  "starved right now" from "starved once, an hour ago", and there was no WARN threshold, HUD
  element, or guidance anywhere telling an agent what value should worry it. `send_starved` is
  the derived answer: `true` exactly while a *sustained burst* of send pressure is happening,
  `false` otherwise — including immediately after a burst ends, even though the two counters it
  is derived from never go back down.
  - **Fire rule:** the client tracks a consecutive-event *streak* — every `send_wouldblock_rescued`
    or `send_deferred` increment bumps it, but only if it lands within 2s of the previous one
    (`SEND_PRESSURE_BURST_GAP_SECS`); a gap longer than that starts a new streak at 1. `send_starved`
    is `true` only once the streak reaches 5 (`SEND_PRESSURE_FIRE_THRESHOLD`) **and** the most recent
    qualifying event is itself still within that same 2s window. The threshold exists specifically so
    a single isolated `WouldBlock` right after `connect()` — documented normal/harmless (#603/#610) —
    never fires it; the recency half exists so the alert is not just "did 5 ever happen close
    together" (which, being derived from ever-growing counters, would itself never clear).
  - **Clear rule:** as soon as the age since the last qualifying event exceeds the 2s window, the
    streak's *size* stops mattering — `send_starved` reads `false` again, whether the burst was 5
    events or 200. This is deliberate: a signal that only ever turns `true` (or never turns `true` at
    all) is the exact anti-pattern the agent-honesty invariant forbids for a *new* observable — worse
    than having no signal, because it looks trustworthy and isn't.
  - Present and boolean on every response (never omitted, never `null`) — an agent should not have
    to distinguish "unalerted" from "field missing".
- **`send_failures_unretried` is the sharper number.** The complement (`send_failures -
  send_failures_unretried`) is the *reliable* stream: a failed reliable datagram is kept verbatim in
  the resend window and retransmitted until the server ACKs it — **for as long as the session
  lives** (see the next bullet; this is a conditional guarantee, not an absolute one). The
  `unretried` ones — unreliable position updates, ACKs, keepalives, session control — are not
  re-sent by this client at all.
- **The reliable stream's guarantee ends when the session does — that is what `reliable_abandoned`
  is for.** `poll_resend` retries indefinitely, but only within one session; when a session ends the
  next one's resend window starts **empty**, so every reliable datagram still outstanding at that
  moment is genuinely lost while `send_failures_unretried` reads `0` for all of them.
  `reliable_abandoned` counts exactly those. It is an **upper bound** on abandoned reliable payload:
  a datagram that reached the wire and whose ACK merely had not arrived yet is counted too.
- **`reliable_abandoned` is measured at 0 across zone handoffs, so treat a nonzero value DURING PLAY
  as signal.** Three consecutive clean handoffs (`qeynos → qeytoqrg → qeynos → freportw`) left it at
  `0` — the resend window was empty at every handoff. (An earlier draft of this page predicted, from
  reasoning and unmeasured, that clean handoffs "routinely leave a small number"; measurement said
  otherwise and the claim is withdrawn.)
- **Clean shutdown is the one measured exception, and is expected to be nonzero.** Two live
  `POST /v1/lifecycle/exit` runs measured `4` and `8`. No agent can observe this anyway — the
  process is exiting — so do not generalise the handoff-measured `0` to this path, and do not read
  the exit value as a fault. (The handoff figure was measured before the shutdown path existed;
  stating both is what keeps these two bullets from contradicting each other, which round-3 review
  N1 caught them doing.)
- **The cause of the exit-time count is NOT established, and this page will not guess at one.** An
  earlier draft claimed the closing OP_Logout / SessionDisconnect were still un-ACKed. That was
  wrong: OP_Logout is a single datagram (so it explains at most 1 of 4-8), and OP_SessionDisconnect
  can never enter the resend window at all — it is framed by the unreliable control path. The two
  measured runs also invert the naive prediction (4 *with* injected reliable traffic, 8 on a control
  run with none). Leftover reliables from earlier in the session are the obvious hypothesis; nobody
  has traced it.
- **What `reliable_abandoned` does and does not cover.** It rises on: zone handoff, world reconnect,
  zone-in failure, and clean shutdown. Since **#642** it ALSO rises on a **server-side session drop
  the client OBSERVES** — inbound `OP_SessionDisconnect`/`OP_OutOfSession`, or a closed socket: the
  client now marks `session_drop` (which forces `connected` false immediately) and the gameplay loop
  tears the phase down, dropping the stream. The one case it still does **not** cover is a server drop
  into *total silence* (no disconnect, no OutOfSession, no ICMP): nothing sets `session_drop`, so the
  stream is not torn down and this stays `0` — for that residual sub-case use `connected`, which goes
  `false` after 15s of link silence.
- **`session_drop`** (**#642**) — `null` while the session is live; a machine-readable cause string
  (`server_disconnect` / `out_of_session` / `socket_closed`) once the client has *positively observed*
  the server end this session. This is the immediate, explicit companion to `connected`: `connected`
  only goes `false` after `CONN_STALE_SECS` (15s) of silence, whereas `session_drop` is set the instant
  the drop is seen on the wire — and when it is set, `connected` is forced `false` regardless of the
  link clock. Read it to distinguish "the server dropped us" from "the world is merely quiet".
- **`send_failures_unretried > 0` does not by itself mean a command was lost, and neither number is
  a complete loss count.** Agent commands travel on the reliable path. `unretried` mixes **two
  classes** that need different diagnoses, and the datagram size is what separates them:
  session-layer control (ACK / OutOfOrderAck / keepalive / session setup — 7-byte datagrams; this is
  what the pre-#641 qeynos burst was, and since #641 those no longer land here) versus unreliable
  `OP_ClientUpdate` position updates.
  **Only the latter means the server's idea of where you are may be stale**; the former stalls the
  server's ordered window instead. The counter alone cannot tell them apart, so do not diagnose a
  subsystem from it on its own. For "did my command get there", the honest reading is the pair
  `connected` + `reliable_abandoned`.
- **A dropped `OP_ClientUpdate` position update is benign-by-supersession, and is deliberately NOT
  deferred — that is a resolved judgement, not an oversight (#655).** When a position update is
  counted here (the position class above), the loss self-heals: each `OP_ClientUpdate` carries the
  full **absolute** x/y/z, and the client re-sends one at most ~280 ms later while moving (a forced
  ~1300 ms keepalive when idle), so the next update carries the current position and fully corrects
  the server's view — the server has no memory of the gap. This was verified against the EQEmu RoF2
  server's position handler: it overwrites the client's authoritative position with each packet's
  absolute x/y/z **unconditionally**, and the wire `sequence` field is written by this client but
  never read back server-side. That same fact is why the transient-refusal *deferral* #641 added for
  control datagrams is **not** extended to position updates: with no sequence or timestamp guard,
  re-sending a stale position on a later tick — after a fresher one had already gone out — would make
  the server apply the older absolute position and **rewind** the character. Deferring here would
  therefore be an agent-honesty regression, not an improvement; dropping and letting the next update
  supersede is the correct behavior. A sustained (not blip) run of position-class failures still
  means the server's idea of where you are is lagging your own — read it with `last_send_error_age_ms`
  and `connected` — but a handful during a CPU-starvation burst is expected to self-heal within one
  send interval. (Pinned by `an_unreliable_position_refusal_is_dropped_not_deferred_and_self_heals`.)
- **Use `last_send_error_age_ms` to tell "one blip at login" from "failing right now."** A count
  alone cannot distinguish them.
- Reported as `null` / `0`, never omitted, so absence of trouble is stated rather than inferred.

**If `snapshot_age_ms` is large, distrust the whole payload.** It means the client's own network
thread has stopped publishing, so every other field is a stale snapshot regardless of what
`connected` says.

### Per-endpoint freshness — `snapshot_age_ms` (#646)

Before #646, only `GET /v1/observe/debug` (`snapshot_age_ms`, above) and `GET /v1/observe/nav_debug`
(`published_age_ms`) carried any freshness signal. Every other `/v1/observe/*` route served its
last-known state with no way for a driving agent to tell it was frozen — the motivating case: with
the `eq-net` thread dead, `GET /v1/observe/entities` kept returning `200` with a frozen entity map
and no marker of any kind (#634/#647).

**It is the SAME clock as `/debug`'s `snapshot_age_ms`, not a new one**: `HttpState::health()`'s
`snapshot_age_ms`, i.e. `NetHealth::last_tick.elapsed()` in milliseconds, computed fresh on every
request (#343 — an age is only true at the instant it's read). `last_tick` is bumped,
unconditionally, once per gameplay tick by the same `eq-net` thread loop iteration that publishes
`GameState` and drains `ActionLoop::tick` — the single writer of every world table these endpoints
read (entities, inventory, chat messages, dialogue choices, doors, zone points, spells, skills,
book text, the packet-telemetry ring). A wedged or dead net thread therefore freezes the data AND
stops bumping this clock in the same instant: a large `snapshot_age_ms` always means "this data can
no longer change", never merely "nothing changed recently".

Two channels carry it, chosen per endpoint by whether the body has room for a new key:

- **A top-level `"snapshot_age_ms"` JSON field**, on every endpoint whose body is already an
  object: `item_text`, `packets`, `inventory`, `messages`, `dialogue`, `spells`, `skills`, `who`,
  and `entities` **only** on its `?labeled=1` shape.
- **The `X-Snapshot-Age-Ms` response header**, carrying the identical value, on endpoints whose
  body is a bare array/map that must keep its exact historical shape for existing consumers (no
  room for a new key without breaking them), or a non-JSON body: `entities`' default `{name:
  [x,y,z]}` map (documented above as backward-compatible for `group_driver.py`), `doors`,
  `zone_entrances`/`zone_points`, `zone_exits`, and `frame` (a PNG). A caller that always checks
  the header never has to know in advance which channel a given endpoint uses.

`/debug` and `/nav_debug` are unchanged by #646 — they already had their own freshness fields
(`snapshot_age_ms` and `published_age_ms` respectively) before this issue.

### `net_thread_dead` — the frozen world's terminality (#634)

Top-level on `GET /v1/observe/debug`, beside `zone_assets` / `common_assets_failed` /
`model_sync_dead`. `null` while the `eq-net` thread — the client's sole writer of world state, sole
drainer of command slots, sole stamper of the health clocks — is running. A **reason string** once it
has ended, for any reason:

| Cause | Reported |
|-------|----------|
| the thread PANICKED | `"the eq-net thread PANICKED (<message>) — …"` |
| a fatal error (login retries exhausted, server-rejected create) | `"the eq-net thread exited with a fatal error (<e>) — …"` |
| it returned with no shutdown requested | `"…returned WITHOUT a shutdown being requested — …"` |
| ordinary `/v1/lifecycle/exit` teardown | `"…exited normally after a shutdown was requested — …"` |
| `--testzone` (offline renderer; no thread was ever started) | `"--testzone: the eq-net thread was never started …"` |

**Read it together with `snapshot_age_ms`, not instead of it.** They answer different questions:

- `snapshot_age_ms` answers *"is this stale?"* — and it is the more general signal, because it also
  catches a thread that is merely wedged, and failure modes nobody enumerated.
- `net_thread_dead` answers *"will it ever un-stale?"* — which no age can, because a 5-second-old
  tick is equally consistent with a busy loop about to recover and with a thread that no longer
  exists. It is also **immediate**: it is set the instant the thread unwinds, whereas
  `snapshot_age_ms` needs 5s to cross the staleness bound and `connected` needs 15s to flip.

When it is non-null, **every world field in the payload is a final frozen snapshot** — position,
zone, entities, vitals — and it will never change again. Stop polling; do not retry commands.
Write commands are refused with `503` naming this field (the reason string is relayed verbatim), so
an agent that ignores it still cannot get a false `200`.

**A live socket does not prove a live world — that's what `world_responsive` is for (#371).** A
wedged zone (its main loop stalled on a script/DB call/deadlock, or merely severely slow) keeps
ACKing our packets, so `connected` stays `true` and `last_packet_age_ms` climbs — *pixel-identical*
to a healthy-but-idle zone, because by construction the failure is "the world stopped producing
output". No passive clock can separate the two. The client resolves it with an **active liveness
probe**: while the world has been application-silent, the network thread periodically sends a
self-`OP_Consider` — a cheap request the zone MAIN LOOP itself must service to answer (no world-server
hop, no faction/aggro side effects, no anti-cheat interaction). If the probe goes unanswered past a
~10s bound while the socket still ACKs, `world_responsive` flips to `false`. An idle-but-alive zone
answers the probe and stays `true`, so this never false-alarms on ordinary quiet. **To decide "is the
world hung", read `world_responsive`, not `last_packet_age_ms`.**

> **Caveat (honest scope).** This EQEmu build runs the zone as a single-threaded libuv loop, so a
> *total* process freeze stops ACKing too and trips `connected: false` as well. What `world_responsive`
> adds over `connected` is detection of a zone that is **still ticking but not making application
> progress** for us (a wedged per-client dispatch, a stuck script, a severely slow tick) — the case
> the passive clocks genuinely cannot see. A `world_responsive: false` is always an honest
> "the zone did not process my app request in time"; it is never a guess.
>
> **Server-content caveat.** The probe relies on the zone replying to a self-`OP_Consider`. A global
> `EVENT_CONSIDER` quest handler that `return`s 1 SUPPRESSES the consider reply
> (`zone/client_packet.cpp` `Handle_OP_Consider`), which on a genuinely idle zone would read as a
> *false* wedge. This is not stock EQEmu and no shipped quest registers such a handler globally — but
> if future server content adds one, it would silently turn every idle session `world_responsive:
> false`. If that signal ever misfires on a known-healthy idle zone, check for a global consider hook
> before trusting it.

---

## Asset sync progress (`GET /v1/observe/asset_sync`) — #715

`zone_assets` (above) tells you whether the world is usable. This tells you whether the load behind
it is **progressing** — the difference between "still downloading, 3 of 7 chunks at 1.2 MB/s" and
"wedged". Before #715 that information existed only on the loading-screen HUD, which an agent cannot
see.

**Idle:**

```jsonc
{
  "active": false,
  "syncs": [],
  // The most recent activity of ANY kind to end — one slot, overwritten by the next one.
  // null only if nothing has ever run in this process.
  "last_ended": {
    "set": "zone/freportw",
    "ago_ms": 4210               // measured at read time
  },
  "semantics": "…"
}
```

**Syncs in flight** — `syncs` lists **every** one, oldest-started first, and the fields of
`syncs[0]` are mirrored onto the top level as the *primary*:

```jsonc
{
  "active": true,
  "syncs": [
    {
      "set": "zone/freportw",    // which set: "zone/<z>", "zonedoors/<z>", "common", "charmodel/<key>"
      // A SET entry's phases. "connecting" is NOT one of them: a login is a different kind of
      // entry, and it never carries a `set` — see "Logins are entries too" below.
      "phase": "downloading",    // "starting" | "verifying" | "downloading"
      "downloading": {           // present ONLY in the downloading phase
        "chunks_done": 3,
        "chunks_total": 7,
        "bytes": 12451840,       // cumulative bytes this downloading phase
        "elapsed_secs": 10.4,    // since this downloading phase began — FROZEN at the last tick
        "rate_bytes_per_sec": 1197292.3
        // …or, instead of a rate: "rate_unavailable": "phase_too_young" | "sample_too_stale"
      },
      "published_age_ms": 120,   // how long ago this sample was published — read this FIRST
      "running_ms": 10520        // how long the whole call has been running, at read time
    },
    { "set": "charmodel/hum", "phase": "starting", "published_age_ms": 4, "running_ms": 4 }
  ],
  "last_ended": null,

  // The one aggregate: the largest published_age_ms over EVERY entry above. Absent when idle.
  "stalest_published_age_ms": 120,

  // …and the same fields as syncs[0], copied verbatim. These describe syncs[0] ALONE:
  "set": "zone/freportw",
  "phase": "downloading",
  "downloading": { /* … */ },
  "published_age_ms": 120,
  "running_ms": 10520,
  "semantics": "…"
}
```

### `published_age_ms` — is it progressing, or wedged?

**Every field above except `published_age_ms` and `running_ms` is frozen at the producer's last
tick, and the producer ticks only when a chunk completes.** A download that hangs mid-chunk — a dead
socket, an asset server that stopped answering — leaves `chunks_done`, `bytes` and `elapsed_secs`
sitting at their last values with nothing left to update them. `elapsed_secs` in particular is *not*
"how long this phase has been running": that is `elapsed_secs + published_age_ms/1000`.

The rate is the one frozen field that is **withheld rather than reported stale** — see
[Why the rate can be absent](#why-the-rate-can-be-absent) below.

Clearing the entry is not the fix — a wedged sync genuinely *is* still in progress, and reporting
"no sync running" would be a worse answer than a stale one. What makes the stale answer honest is
its age. `published_age_ms` is computed **at read time**, so:

- **small, and staying small across polls** → the loader is ticking; the numbers beside it are current.
- **large and growing** → the sync is **wedged**. The numbers beside it describe the last moment it
  was alive, not now, and the rate in particular is meaningless.

A chunk can legitimately take a while, so a single large reading is not proof of a stall — a value
that keeps growing across polls is. Do not treat `chunks_done` as current without checking it.

`running_ms` is the companion number: how long the whole `sync_set` call has been running, also
measured at read time. Unlike `downloading.elapsed_secs` it keeps **moving** while a sync is wedged,
and it is the only duration that exists at all in the `starting` phase, where a hung manifest fetch
has no chunks, no bytes and no elapsed. A large `published_age_ms` says *this sample is old*;
`running_ms` says *and the call has been going this long*.

### The two distinctions this shape exists to keep

**`active: false` is not "zero progress".** An idle client and a download stuck at 0 of 7 chunks are
different situations an agent acts on differently, so they are different bodies: idle carries
`"active": false` and **no** `set`, `phase` or `downloading` key at all, while a stalled download is
`"active": true` with `downloading.chunks_done: 0`. Branch on `active` — never on a count being zero.

**The phase is modelled, not flattened.** Transfer data lives inside `downloading`, an object that
exists **only** in that phase. This mirrors the producer, where a rate is structurally
unrepresentable outside downloading (#708). A flat body with a nullable `rate` would make "not
downloading" indistinguishable from "downloading, rate not derivable yet"; here they are different
structures, not two spellings of `null`.

### Why the rate can be absent

`rate_bytes_per_sec` is **omitted** whenever it cannot be stated honestly — never `null`, and
certainly never `0`, which would read as "the download has stopped". Its absence is unambiguous
because the enclosing `downloading` object is present, and `rate_unavailable` always names the rule
that withheld it:

| `rate_unavailable` | Meaning |
|---|---|
| `phase_too_young` | The downloading phase is under 100 ms old — the same minimum-elapsed guard the HUD's speed line uses. No honest rate can be divided out of that yet. |
| `sample_too_stale` | Nothing has ticked for over **2000 ms**. The last rate has stopped being an assertion about *now*. |

The staleness rule exists because this endpoint can be polled minutes after the sample was taken,
which the HUD never is: it redraws from the tick that produced the number. The scale of the problem,
from a live zone download on this client: the last completed tick measured 31,294,024 B over
20.65 s, i.e. 1,515,404 B/s. Without the rule the endpoint keeps asserting that figure for as long
as the sync is wedged — five minutes in, the phase's actual average is 31,294,024 B / 320.65 s
≈ 97,600 B/s and the instantaneous rate is zero, a 15× overstatement published with no marker.

**How 2000 ms was chosen.** Inter-tick gaps measured on the same healthy live download: median
41 ms, p95 185 ms, **max 469 ms**. A second cold-cache run polling this endpoint through a full
Neriak load (five sets, three of them concurrent, ~1,400 chunks / ~110 MB) saw a worst `published_age_ms`
of **491 ms** and never once withheld a rate. The threshold is ~4× that worst healthy gap, so a link
four times slower than the measured one still publishes a rate continuously. The error is deliberately
asymmetric: withholding a rate costs a caller nothing, because `bytes`, `elapsed_secs` and
`published_age_ms` are all still there and `bytes / (elapsed_secs + published_age_ms/1000)` is an
honest **lower bound**; publishing a stale one is a falsehood no caller can detect from the number
itself.

### Phases

- **`connecting`** — an asset-server **login** is in flight (#731). Not a set sync at all: this entry
  has no `set` and no `downloading`, and carries a `connecting.purpose` instead. See
  [Logins are entries too](#logins-are-entries-too-and-they-are-not-transfers--731).
- **`starting`** — the `sync_set` call has begun; its manifest request is in flight and the producer
  has not reported a phase yet. This phase is published by the client, not by the sync producer, and
  it exists so the "a sync is running" window covers the whole call: leaving it `active: false`
  would report "no sync in progress" while one demonstrably was. A set that turns out to be already
  up to date is only ever seen in this phase.
- **`verifying`** — the producer's `SyncProgress::Verifying`, emitted once the manifest has been
  accepted and before any chunk transfer begins. Carries no transfer data.
- **`downloading`** — fetching chunks. Ticks once per fetched chunk, so `chunks_done`/`chunks_total`
  count the chunks this sync actually has to transfer, not the set's total file count. A set whose
  chunks are all already cached emits no downloading tick at all rather than a misleading 0 B/s one
  (#708). The only phase with transfer data.

### When an entry is REMOVED

Each entry is written by an RAII guard wrapped around it, so the entry disappears when that
sync (or login) **succeeds**, when it **fails**, and when the loader thread **panics** mid-call (the guard
removes it on unwind, so there is no "clear at the end of the happy path" to be skipped). No exit
path can leave a *finished* sync reported as in-flight.

The wrappers that own those guards — `sync_set_observed` and `login_observed` — are the only way to
reach `sync_set` and `AssetSync::login` **from anywhere outside `src/asset_sync.rs`**, where both are
private. Every production call site is outside it, so for all of them an unobserved sync or login is
a compile error rather than a thing to remember (#726 review N3, #731). The privacy is module-scoped,
not crate-scoped: code added *inside that one file* can still call them directly, so the compiler
covers every caller but that file, and review covers that file (#743 review N5).

A sync that never exits at all — a hung transfer — is the case a guard cannot address, because it
has not finished and reporting it as finished would be its own falsehood. That one is covered by
[`published_age_ms`](#published_age_ms--is-it-progressing-or-wedged) above, not by removal.

`last_ended` names the most recent activity **of any kind** to leave the list, so `active: false`
right after a load is distinguishable from `active: false` in a process where nothing has ever synced
(`last_ended: null`).
For a **set sync** it says **ended, not succeeded**: `Drop` runs identically on the success return, the error return and
a panic unwind, and genuinely cannot tell them apart. Do not read it as "the set is now cached".
For a **login** it does carry a verdict — see [Logins are entries too](#logins-are-entries-too-and-they-are-not-transfers--731).

**`last_ended` is one slot, and it is overwritten by everything.** Logins and set syncs share it, so
whatever is in it survives only until the *next* activity of either kind ends. During a zone load
that is milliseconds: two set syncs end per zone, plus a `charmodel` set whenever an unseen race
appears. It answers "what ended most recently"; it does **not** answer "did X happen at some point",
for any X. For the login-failure case specifically, use `login_outcomes` / `last_login_failed`
described [below](#logins-are-entries-too-and-they-are-not-transfers--731).

### Overlap, and what `syncs` is for

The client runs several syncs concurrently: the zone's terrain (`zone/<zone>`) and door models
(`zonedoors/<zone>`) during a zone load, the shared `common` set once the window comes up, and
`charmodel/<key>` sets on demand as an unseen race model is first needed. A short `charmodel` sync
routinely begins **and ends inside** a long zone download.

Every one of them gets its **own entry**, owned by its own guard, which can only ever remove its
own. A finishing sync therefore cannot blank out a different one that is still running — in either
direction, whether it started earlier or later. That is why `active: false` means *no asset sync is
running in this process*, subject to the one startup exclusion below.

`syncs` is ordered **oldest-started first**, and the fields of `syncs[0]` are mirrored onto the top
level as the *primary*, for callers that do not want to iterate to find out whether anything is
running at all. The mirror is a copy of the same encoded object, so the two can never disagree.

**The primary describes one sync — the one `set` names — and nothing else.** Oldest-started is
chosen because it is *stable*: it does not change identity when a sibling's age or rate changes, so
a caller polling `set` sees the same sync from poll to poll. It is deliberately **not** chosen as
"the sync that matters", because the client cannot know which sync the caller is waiting on, and a
guess dressed as an answer is exactly the failure this endpoint exists to avoid. To follow a
particular set, find it **by name** in `syncs`.

> ⚠️ **Correction (#726 review, round 2).** This section previously read: *"`syncs[0]` — the
> long-running one an agent is waiting on, and the one whose wedge matters — is mirrored onto the
> top level so a caller that only wants 'is the load I am waiting on alive?' need not iterate."*
> That is retracted. Two claims in it are false. (1) `syncs[0]` is not always the long zone
> download: a `charmodel/<key>` sync queued during the *previous* zone can still be in flight when
> the next `zone/<zone>` opens, and is then the older of the two. (2) A healthy primary does **not**
> mean the process is healthy — a sibling can be wedged for minutes behind it. No field value was
> ever wrong; the guidance was, and it shipped inside the response's own `semantics` string.

**`stalest_published_age_ms` — the one-field wedge check.** The largest `published_age_ms` over
**every** live entry, **including logins** (#731). Because it is a maximum, it is large and growing whenever *any* sync is wedged,
whatever `syncs[0]` is doing, so it is the field to poll when the question is "is this process stuck"
rather than "how is set X doing". It is taken from the ages already encoded in `syncs` — never
re-measured — so it always equals one of them exactly. It is **absent, not zero**, when nothing is
running: a maximum over no samples is not a measurement, and `0` would read as "everything is
perfectly fresh". Read `active` to tell idle from fresh.

**Startup exclusion.** The `gamedata` and `gameequip` sets are synced during early startup, **before
this HTTP server binds its port**. They are recorded in the registry like any other sync, but
nothing can poll the endpoint until after they finish, so they are effectively invisible. On a cold
cache that window was measured at **349 s** (launch to port bind, empty asset cache, one run) — a
caller waiting for the API port to open should expect a multi-minute wait on first run, and cannot
distinguish it from a hung launch through this endpoint. It is not a hole in `active`: by the time
anything can reach this endpoint, they are done.

They are also not readable from `last_ended` in general, for the reason above: it holds only the most
recent activity, and the first thing the client does after binding the port is start more of them —
the reviewer's live run saw the pre-bind startup login pushed out of that slot within seconds. The
startup **login**'s verdict does survive, in `login_outcomes` / `last_login_<outcome>`; the two pre-bind
**set syncs** have no equivalent, so whether they succeeded is not observable through this endpoint
at all.

### Logins are entries too, and they are not transfers — #731

Every `sync_set` is preceded by an asset-server `login()`. Until #731 that call sat **outside** the
guarded window, so a login that was slow, hung, or retrying against an unreachable asset server made
this endpoint answer `{"active": false}` — "no asset sync is running" — while a loader thread was
blocked inside it. The HUD said "Verifying zone assets…"; the API said idle.

A login in flight is now an entry:

```jsonc
{
  "active": true,
  "syncs": [
    {
      "phase": "connecting",
      "connecting": { "purpose": "zone load: neriakc" },
      "published_age_ms": 8140,
      "running_ms": 8140
      // note: NO "set", and NO "downloading"
    }
  ],
  "stalest_published_age_ms": 8140,
  "phase": "connecting",                        // …mirrored, as syncs[0]
  "connecting": { "purpose": "zone load: neriakc" },
  "published_age_ms": 8140,
  "running_ms": 8140,
  "last_ended": null,
  "semantics": "…"
}
```

**Why `connecting` is its own kind of entry rather than a fourth sync phase.** A phase would need a
`set`, and **three of the client's four logins do not have one**: the model-sync worker logs in once
and then serves an unbounded queue of `charmodel/<key>` sets, startup logs in once for `gamedata`
*and* `gameequip`, and the zone loader's single login covers both `zone/<z>` and `zonedoors/<z>`.
Only `common` is 1:1. Filling `set` in at the other three would be a guess, and a caller looking that
set up in `syncs` would find it and read a transfer that had not started.

**Why it carries no transfer data.** A login has no set, no chunk count, no byte total and no rate.
Reporting it through the sync shape would make it read as *a download stalled at 0 bytes* — which is
a subtler version of the same falsehood, not a fix for it. `set` and `downloading` are therefore
**absent** on a `connecting` entry (and absent from the top-level mirror when `syncs[0]` is a login),
following the same absent-not-zero rule as the rate.

**`purpose` is free text, not a set name**, and is deliberately not set-shaped (`"zone load:
neriakc"`, not `"zone/neriakc"`). It says what the login is for; it is not a lookup key.

**A hung login shows up in `stalest_published_age_ms`** with no special case. A login is one atomic
request: it publishes exactly once, at `begin`, and never ticks — structurally identical to a sync
wedged in `starting`. Its `published_age_ms` is therefore the whole time it has been blocked, and
the documented one-field wedge check is large and growing throughout. `running_ms` and
`published_age_ms` are the same duration for a login, for the same reason.

### Did a login fail? — `login_outcomes` and `last_login_<outcome>`

**A login's verdict is measured, not guessed.** `login_observed` wraps the call and sees its
`Result`. (A set sync's `Drop` runs identically on success, error and unwind and genuinely cannot
tell them apart — that limitation is unchanged.) `unknown` means a panic unwound through the login,
so it neither returned `Ok` nor returned `Err`. Without a verdict at all, a *failed* login and a
*successful* one are the same `active: false`, which is #731's falsehood reappearing a moment later.

The verdict is published in **three places with three different retentions**, and the difference is
the whole point:

```jsonc
// Always present. Counts of logins that have ENDED, by outcome. Monotonic WITHIN ONE PROCESS.
"login_outcomes": { "succeeded": 2, "failed": 1, "unknown": 0 },

// ONE SLOT PER OUTCOME — `last_login_<outcome>` for each key in `login_outcomes` above.
// Each is present exactly when its counter is non-zero, and each is overwritten only by a LATER
// login with the SAME outcome. A failure and a panic never compete: neither can hide the other.
"last_login_failed": {
  "connecting": { "purpose": "common asset load", "outcome": "failed" },
  "ago_ms": 41220
  // note: NO "set" — a login never had one
},
"last_login_succeeded": { "connecting": { "purpose": "zone load: qeynos2",
                                          "outcome": "succeeded" }, "ago_ms": 210 },
// "last_login_unknown" would appear here too, had a panic ever unwound through a login.

// The most recent activity of ANY kind to end. Overwritten by the next one, login or set sync.
"last_ended": {
  "connecting": { "purpose": "zone load: neriakc", "outcome": "failed" }, // succeeded|failed|unknown
  "ago_ms": 210
}
```

**Ask `login_outcomes` — not `last_ended` — whether a login failed.** `failed + unknown > 0` means a
login did not complete in this process, at *any* polling cadence. `last_login_failed` /
`last_login_unknown` then name which one and how long ago, each within its own outcome. Logins still
in flight are counted in none of them: they are live entries in `syncs`.

**Monotonic means monotonic *within one client process*.** The counters start again at zero when the
client restarts, which it does routinely (`POST /v1/lifecycle/exit`, crash, relaunch), and this body
carries no restart marker. So the delta between two polls is what happened in between **only if the
client did not restart between them** — a poller keeping a cursor across restarts must read a
*decrease* as a new process, not as a correction. The served `semantics` string says this too, since
that is what an agent actually reads.

> ## ⚠️ Corrections (#743 review, round 2)
>
> **Retracted.** This section previously offered this row in its four-state table:
>
> | the client has… | the endpoint says |
> |---|---|
> | had a login fail | `active: false`, `last_ended.connecting.outcome == "failed"` |
>
> **That recipe does not work, and it fails in the direction that matters most.** `last_ended` is a
> single last-writer-wins slot shared by logins *and* set syncs, so a login's verdict is destroyed by
> the very next activity of either kind to end. The reviewer measured it on a live run where **all
> four logins failed**: across **75 polls at 1.5 s**, the genuinely-failed `common asset load` login
> appeared in `last_ended` **0 times** — it was overwritten before any poll could see it, by
> `startup game data`, then `model-sync worker`, then `zone load: neriakc`. An agent following the
> retracted row would have read `last_ended` naming some other activity and concluded **no login
> failed, when three had.** Every individual field was accurate; the guidance was a confident
> falsehood.
>
> The same round retracts the paired row *"login succeeded → …then the set sync it enabled opens its
> own entry"* — see the disclosed gap below, where that promise is false at one of the four sites.
>
> `login_outcomes` and the `last_login_<outcome>` records were added in response, so the question the
> retracted row claimed to answer now has fields that really answer it.

The states an agent can distinguish:

| the client has… | the endpoint says |
|---|---|
| never tried | `active: false`, `syncs: []`, `last_ended: null`, `login_outcomes` all zero, no `last_login_*` |
| a login in flight | `active: true`, `syncs[i].phase == "connecting"`, `stalest_published_age_ms` present and growing |
| had a login **succeed**, ever | `login_outcomes.succeeded > 0` ⟺ `last_login_succeeded` present, naming the most recent one |
| had a login **fail**, ever | `login_outcomes.failed > 0` ⟺ `last_login_failed` present, naming the most recent **failed** login |
| had a login **panic**, ever | `login_outcomes.unknown > 0` ⟺ `last_login_unknown` present, naming the most recent **panicked** login |
| had *any* login not complete | `login_outcomes.failed + .unknown > 0`; the two records above name both, independently |
| just ended *something* | `last_ended` names it — and only it. Not a history, not a search |

Those ⟺ rows are the contract — one for **every** key `login_outcomes` carries, which is the list to
read the outcome set off rather than this table — and each is *per outcome*: the record beside a counter always
describes a login that ended **that** way. A second failure does replace the first failure's
`purpose` (these are "most recent per outcome", not a log) — one question, one answer, and both
still counted.

> ## ⚠️ Correction (#743 review, round 3): `last_login_failure` was ONE slot for TWO categories
>
> **Retracted, and fixed in code rather than in wording.** Round 2 served a single
> `last_login_failure` holding the most recent login that did not succeed *of either kind*, and these
> two rows offered it to two different questions:
>
> | the client has… | the endpoint says |
> |---|---|
> | had a login fail, **ever** | `login_outcomes.failed > 0`; `last_login_failure` names the most recent one |
> | had a login panic, **ever** | `login_outcomes.unknown > 0`; it is in `last_login_failure` too, with `outcome: "unknown"` |
>
> **At most one of those could be true at a time.** The reviewer proved it with two probes, both RED
> against that body:
>
> - panic, then failure → `unknown == 1`, but `last_login_failure` named the **failed** login
> - failure, then panic → `failed == 1`, but `last_login_failure` named the **panicked** login
>
> **The harm:** an agent sees `unknown > 0`, follows the row, and reads *a different login's*
> `purpose` — attributing the panic to a login that did not panic. And the superseded login's
> identity was recoverable from the endpoint **nowhere at all**. That is round 1's B1 shape one level
> down, inside the fix for B1: one observable, two meanings.
>
> **What changed.** The remedy is not a caveat telling readers to check an `outcome` field. There is
> now **one slot per outcome**, so both rows above are true simultaneously and neither login is lost.
> Structurally, in `crates/eqoxide-ipc/src/asset_sync.rs`:
>
> - `LastLoginByOutcome` reaches its slots only through an exhaustive `match` on `ConnectOutcome`, so
>   which slot a record lands in is a function of the outcome and not a choice at the call site;
> - the retained `RetainedLogin` **carries no outcome field at all** — the outcome is *which slot it
>   is in*, so a record cannot claim an outcome that disagrees with the category it answers for; and
> - the encoder derives the wire field name, the `outcome` token inside it and the counter key from
>   the same `outcome.as_str()`, in one pass, so no category can be counted without its record being
>   served beside it.
>
> Both of the reviewer's probes are now shipped as tests (`crates/eqoxide-ipc/src/asset_sync.rs` and
> `crates/eqoxide-http/src/observe.rs`) and both pass, with the *earlier* login recoverable in full
> in both orderings — the assertion neither probe could make against round 2.

**Known gap, disclosed — stated as a rule, because the instance is not the class.** A login's entry
ends the moment `login()` returns; the sync it enables opens its own entry only when the next
`sync_set_observed` call runs. In between, `active` is `false` while the pipeline is alive.

> **The rule: the width of that window is whatever stands between those two calls, and it is
> unbounded wherever the next `sync_set_observed` is gated on a blocking receive.** Do not read this
> gap as "brief" in general; read the code between the login and the sync at the site you care about.

- **Three of the four sites** (`src/main.rs` startup, and the zone loader and common loader in
  `src/app.rs`) have only a status-string write and a clone between the two calls — no I/O, no
  blocking. A poll can land there and see `active: false`; it cannot get *stuck* there.
- **The model-sync worker (`src/app.rs`) is the unbounded instance.** Its next `sync_set_observed` is
  inside `while let Ok(key) = model_rx.recv()` — an unbounded receive with no timeout. On a client
  that meets no new race, that receive never returns and the worker sits in this window **for the
  rest of the session**. `active: false` is honest there: nothing *is* running. What is not honest is
  promising a sync will follow. **No sync is promised**, and an agent that waits for one may wait
  forever.

Closing the gap would need one guard spanning login and sync, which does not generalise to the three
logins that serve more than one set — and at the model-sync worker there is no "the sync" to span to.

*Epistemic status:* the mechanism at all four sites is **read from the code and certain** — the
statements between the two calls, and the unbounded `recv()`, are both plainly there. The *durations*
at the three bounded sites are **inferred from the absence of I/O and blocking in that code, not
timed**; nothing here instrumented them, and the live runs that could have were pointed at an
unreachable asset server, so no success path ever ran (#743 review B2).

---

## Nav diagnostics (`GET /v1/observe/nav_debug`) — #608

The full nav diagnostics snapshot navigation **publishes** — the same single source of truth the
in-client depth-tested 3D overlay (F11 / `--nav-debug`) draws. The driving agent has no eyes, so
the snapshot is served here in structured form; the JSON body is a structural serde projection of
the nav-owned snapshot type (`eqoxide_nav::diagnostics::NavDebugSnapshot`), so a field cannot
silently diverge from what nav published. **Nothing in this endpoint (or the overlay) re-derives
nav state** — no floor queries, no clearance re-checks; consumers render what the planner and
walker actually decided.

Top-level shape (`available: false` + a `note` until the walker first publishes):

- **`seq`** — monotonic publish counter.
- **`published_age_ms`** — ms since the walker published this snapshot, computed AT READ TIME
  (like `/debug`'s `snapshot_age_ms`). The idle walker republishes whenever a published fact
  drifts (the player moves, the zone model loads), so a growing age on a live client means the
  state genuinely has not changed.
- **`zone_model_loaded`** — whether the walker HAS a collision grid for this zone. `false` = no
  world model: nothing here is a claim about geometry (#579). The composed **`zone_assets`**
  object (same source as `/debug`'s) rides along for the pending/failed/stale detail.
- **`nav_state` / `nav_reason`** — the walker's published state at publish time (same vocabulary
  as `/debug`).
- **`goal_id`** — the live `nav_goal_id` (#349) at publish time. It is what makes `plan` below
  attributable (#631): `plan` **survives route clears** (it is the diagnostic OF a failure), so after
  a `/stop` or a fresh goto the previous goal's plan keeps riding the snapshot. When
  **`plan.goal_id != goal_id` the plan describes a SUPERSEDED command**, not the current one — never
  read `plan.gen`/`plan.outcome` as this command's result unless the two ids match. (Before this, the
  retained plan carried no identity and a failed goto's record read as the current command's outcome.)
- **`player`**, **`goal`** — position `[east,north,up]` at publish (**`null` when the position was
  not known** — fresh login before the first server placement, a zone reset; never a fabricated
  `[0,0,0]`); the active `/goto` goal.
- **`committed_coarse` / `committed_fine`** — the walker's **actual committed** coarse route and
  fine/local steering plan, verbatim (`Walker::path`/`local_path` — the #246 property; never a
  recompute).
- **`plan`** — the last coarse plan's record, from the planner's own reply: `gen`, **`goal_id`**
  (the command this plan ANSWERS — compare against the top-level `goal_id` to tell a current plan
  from a superseded one, #631), `start`, `goal` (the question actually asked), `outcome`
  (`route`/`unreachable`/`exhausted`), `reason` (the `nav_reason` vocabulary), `route_len`,
  `plan_ms`, `tight`, `goal_snapped` (the VERTICAL goal change, #344), **`goal_offset`** (the
  HORIZONTAL companion, #631: the east/north distance in units from the goal you NAMED to where the
  committed route actually ENDS — `0.0` for a complete route to the goal, nonzero for a partial that
  stops short; so `goal_snapped: false` can no longer hide that the destination differs horizontally
  from your coordinates — the #482 "planned 55u away" observation), and **`trace`**:
  - `trace.calls[]` — one entry per A* call (clearance tier × anchor attempt), each with
    `clearance`, `cell`, `char_anchor`, `truncated`, and `edges[]`;
  - each edge: `{from, to, verdict: "accepted", kind}` or `{from, to, verdict: "rejected",
    reason}` with reasons `no_floor | step_up | step_down | grade | clearance | water |
    haul_out_too_high` — recorded **at the branch that made the decision**, in the search itself;
  - `trace.outcome_calls` — the `[i, i+1)` range of **the DECIDING call**: the one A* call whose
    result became the returned outcome. Tier retries (a generous pass a minimum pass superseded),
    anchor retries and re-anchor-ring attempts that lost are still present in `calls[]` (with
    their `clearance`/`char_anchor` metadata) but sit OUTSIDE this range — the overlay draws only
    the deciding call, so a losing pass's rejections are never painted over the committed route;
  - `truncated: true` on a call = the RECORDING budget ran out (total per plan, and at most half
    per call so the deciding call is never starved by an earlier flood) — **the search itself was
    not cut short**, and the recording boundary is NOT the planner's frontier. The overlay marks
    the spot recording stopped with an orange double-ring + beacon.

  **Honesty contract: absence means UNEVALUATED.** A cell or edge missing from the trace was
  never evaluated by the planner. It is neither walkable nor blocked; consumers must not fill in
  gaps, and the response's `semantics` field restates this on the wire.
- **`pads[]`** — same-zone teleport-pad knowledge (#543/#266): `{index, knowledge}` where
  `knowledge` ∈ `unknown` (no usable advertised destination; never observed) /
  `advertised_usable` (+`source`,`dest`; wire-advertised and honesty-gated onto walkable floor —
  **advertised, not verified**) / `advertised_unusable` (advertised but refused by the gate) /
  `learned_same_zone` / `learned_cross_zone` (reserved for the #543 learning loop). "Not yet
  observed" is first-class and never collapses into an answer.
- **`clearance`** — a throttled live sample of nav's own traversability model at `at` (which may
  lag the player a few ticks): 16 radial `wall_spokes` (saturating at `cap`), the 8-direction
  `footprint_ok` ring at `footprint_radius`, and the zone-lifetime field values `field_wall` /
  `field_ground` the planner's hug-cost/margin actually consult.
- **`water`** — the swim state the walker acted on this tick (`swimming`, `swim_plane`), i.e. the
  values that went into its MoveIntent — not a recomputation.

---

## Nav footing verification (`nav_support`)

`GET /v1/observe/debug` also carries **`nav_support`** (top-level, not under `player`) — whether pathing in the current zone is
answering from **winding-blind (inverted-art) ground**. **`null` means every standable surface so far
faced UP** (properly wound); an object means nav has answered from a down-facing surface:

```json
"nav_support": {
  "reason":  "facing_blind_ground",
  "queries": 412,
  "detail":  "parts of this zone's collision mesh are wound INVERTED ..."
}
```

Since **D-2 (#375)** nav's floor predicate `is_standable` is **facing-blind**: a surface is ground on
its flatness + headroom, whichever way its art is wound — because some zones bake real, walkable
ground from **inverted (down-facing) art** (the qcat live wedge stood on exactly such a walkway, which
the old up-facing-only filter deleted). Those surfaces ARE walkable, but nav can no longer *verify*
their facing, so `nav_support` counts each query answered from one.

> **Renamed from `nav_degraded`/`inverted_floor_art`.** That older signal counted a `column_bottom`
> recovery valve, which D-2 removed. Had it been left reading the dead counter it would report `null`
> ("all pathing on properly-wound floors") in exactly the inverted-art zones (permafrost/highpass/
> neriakc/qcat) where nav is now on winding-blind ground — a confident falsehood. The signal moved
> with the mechanism so it stays honest.

`queries` counts how many nav queries have been answered from down-facing ground since the zone
loaded. Read `nav_support != null` as *"footing here is unverified-winding"* — not an error and not a
routing failure (the ground is walkable), just an honest "this footing's facing is unconfirmed."

## Consider results

A consider (`POST /v1/combat/consider {"id":N}`, default current target) tells you two independent
things about a spawn: its **attitude** (faction-derived — how it feels about you) and its
**difficulty tier** (level-derived — how tough the fight would be). `GET /v1/observe/debug` surfaces
both, on two different fields depending on whether the considered spawn IS your current target:

- **`player.target_con` / `player.target_attitude` / `player.target_level`** (#292) — describe the
  **CURRENT target only**. These are `null` whenever nothing is targeted, or when the consider reply
  was about a *different* spawn (#330 — a stale reply can never overwrite the current target's con).
- **`last_consider`** (#336, top-level, not under `player`) — describes the **most recently
  considered spawn, target or not**. This is what makes a *standalone* consider (a spawn deliberately
  NOT your target) readable: `POST /v1/combat/consider {"id":N}` for a non-target spawn always
  populates this, even though it leaves `target_con`/`target_attitude`/`target_level` untouched.

```json
"last_consider": {
  "spawn_id": 450,
  "name": "Guard_Phaeton",
  "con_name": "red",
  "attitude": "scowls",
  "level": 20,
  "ago_secs": 2
}
```

`con_name` — the **difficulty tier**, from the RoF2 `Consider_Struct`'s `level` field (an EQEmu
`ConsiderColor` enum value, not a literal level number):

Ordered from safest to deadliest (by the mob's level relative to yours — `gray`/`green`/`light_blue`/
`blue` are all **beneath** you, `white` is **even**, `yellow`/`red` are **above** you):

| `con_name`   | ConsiderColor | Meaning |
|--------------|---------------|---------|
| `gray`       | 6             | Far beneath you — trivial, no experience for the kill. |
| `green`      | 2             | Well beneath you — safe. |
| `light_blue` | 18            | Beneath you (further below than `blue`, closer to `green`). |
| `blue`       | 4             | Just beneath you — nearly even, but still below your level. |
| `white`      | 10 / 20       | Even con — same level as you. |
| `yellow`     | 15            | Above you — noticeably higher, dangerous. |
| `red`        | 13            | Well above you — much higher, likely lethal. |

`attitude` — the spawn's **faction disposition**, from the reply's `faction` field (`1..=9`): `ally`,
`warmly`, `kindly`, `amiable`, `indifferent`, `apprehensive`, `dubious`, `threatening`, `scowls`
(ready to attack / KOS). This is entirely independent of `con_name` — a low-level mob can still
`scowls` at you (hostile *and* trivial), and a high-level mob can be `ally` (friendly *and* lethal if
it ever turned on you). Never infer one from the other.

`level` is the spawn's actual character level (from its spawn record), when known — `null` is an
honest "unknown" (e.g. it had already left the entity table by the time the reply arrived), never a
guessed number.
