# eqoxide HTTP API

The agent-facing REST API. eqoxide itself no longer listens on TCP: start it with
`--agent-socket PATH` and run the [harness-http](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar) wrapper against that socket
(`harness-http PATH [bind-addr]`, default `127.0.0.1:8090`). The wrapper serves these routes and
forwards the ones it does not handle itself to eqoxide as `Call`s over the socket (see
`agent-api.md`), so every route below answers on the harness's address.

`docs/specs/2026-09-21-agent-harness-separation-design.md` plans to move this entire API out of
eqoxide: navigation and combat-positioning (`goto`, `follow`, zone-cross, auto-engage) live in a
harness project built on the
[Agent Plugin API](agent-api.md), and the rest get an `AgentVerb`/`Observation` equivalent on that
socket. This document describes what's implemented today; a new integration should prefer the
Agent Plugin API where it already covers the needed capability.

## Versioning & grouping

All routes are **versioned and grouped**: `/<version>/<group>/<action>`. The current version is
**`v1`**. Groups mirror the agent's mental model (and the `eqoxide_mcp` tool grouping):

| Group | Purpose |
|-------|---------|
| `observe`   | read-only world/player state |
| `move`      | raw movement: manual wish vector, jump |
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
working. The implementation lives in `crates/eqoxide-agent-ops/src/<group>.rs`, each registering
its routes as named ops (`"POST /v1/<group>/<action>"`) that the agent socket runs; eqoxide itself
contains no HTTP server.

---

## `observe` — read-only state

| Route | Description |
|-------|-------------|
| `GET /v1/observe/inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency, coin_verified, snapshot_age_ms}`. Slots are Titanium **wire** ids (DB general slots 23-30 are one lower on the wire, starting at 22). |
| `GET /v1/observe/messages[?kind=npc]` | Machine-readable message log (oldest→newest). `{count, messages, snapshot_age_ms}`; each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `GET /v1/observe/dialogue` | Pending NPC dialogue/quest choices `{count, choices:[{index, text}], snapshot_age_ms}`. |
| `GET /v1/observe/spells` | The 9 memorized gems `{gems:[{gem, spell_id, name, mana_cost, cast_time_ms, recast_time_ms}], snapshot_age_ms}` (empty gem, or an id our spell table has no row for → all of `name`/`mana_cost`/`cast_time_ms`/`recast_time_ms` null). See [`mana_cost` / `cast_time_ms` / `recast_time_ms`](#mana_cost--cast_time_ms--recast_time_ms-per-gem--resource-costcooldown-1127). |
| `GET /v1/observe/skills` | All skills with current trained value `{skills:[{id, name, value}], snapshot_age_ms}`; `value == 0` means untrained. |
| `GET /v1/observe/doors` | Current zone's doors — `{doors:[{door_id,name,x,y,z,heading,opentype,is_open}], snapshot_age_ms}`. **An empty `doors` does not mean "this zone has no doors"** — the roster is server-pushed and zone-in empties it, so an empty roster is "no record held", not "none exist"; see [`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073). |
| `GET /v1/observe/zone_entrances` | Zone entrance points received from the server (arrival side), plus a handful of client-synthesized entries read from the CURRENT zone's own map (the heuristic only ever recognizes a label naming North/South Qeynos or Qeynos2, but — measured — five zones' shipped map packs actually carry such a label: see [`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816) for the list and method). Also served at the deprecated alias `GET /v1/observe/zone_points`. Served as `{zone_entrances:[…], snapshot_age_ms}`. **If those synthesized entries failed to load, this list is silently short** — check [`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816) on `/v1/observe/debug`. Separately from that load gap, **an empty `zone_entrances` does not mean "this zone has no entrances"**: zone-in empties this list too (#1010/#1063) and the server-advertised entries refill on no schedule the client controls — see [`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073). |
| `GET /v1/observe/zone_exits` | Current zone's exits (the WLD zone-line regions you navigate toward — see [`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579) for its 503 gating). An entry with `"zone_id": null` is a REAL exit region whose baked index matches no advertised zone point (#683) — its destination is honestly unknown until the server resolves a crossing there. Crossing it works **only if this zone advertises no same-zone teleport point and server zone points are available** (received and not all filtered client-side) — the same #679 gate that keeps the client from firing a blind crossing off an intra-zone pad. In a gated zone (e.g. one with teleport pads), standing on a `zone_id: null` exit does NOT cross. **Each entry carries `"gated": true|false` (#713)** — it reports the **#679/#683 zone-level** unresolved-cross gate and nothing else, so you can see that refusal **before** walking there instead of reading about it in the message log after. **`gated` is a property of the zone and the entry, not of you**: your position is not an input to it. `gated` is only ever `true` for `zone_id: null` entries (an advertised destination is crossed directly and is never subject to the #679 gate); it **reports** the gate verdict and does not change it. **`gated: false` is not a promise the auto-cross will fire.** It says only that this gate is open — the stand-scoped [#713 attempt bound](#zone-cross-degradations-you-can-detect-713) can independently have stopped auto-crossing, and `zone_exits` never consults it, so cross-check `zone_cross_stopped` on `/v1/observe/debug` before concluding an exit is broken. The message-log line ("auto-cross is disabled here", once per stand) is still emitted for callers that watch events. Served as `{zone_exits:[…], snapshot_age_ms}`. |
| `GET /v1/observe/item_text` | Text of the most recently read book/note `{text, snapshot_age_ms}` (`text: null` if none read this session). |
| `GET /v1/observe/who` | Server-wide `/who all` roster `{online:[{name, level, class, race, zone_id, guild, anon}], snapshot_age_ms}`. 503 if no response arrives in time. |

Every route above that lacked ANY freshness signal before #646 now carries one — either a
top-level `"snapshot_age_ms"` JSON field or, where the body is a bare array/map/PNG that cannot
safely gain a new key, the `X-Snapshot-Age-Ms` response header. See
[Per-endpoint freshness](#per-endpoint-freshness--snapshot_age_ms-646) for the full field-vs-header
breakdown and why.

---

## `move` — movement

| Route | Body | Description |
|-------|------|-------------|

**`manual` and `jump` answer `409 Conflict` with JSON
`{"status":"held", "hold":{…}, "message":…}` while the character is physics-held in a way that
freezes the controller's step (`player.hold.reason == "embedded_no_recovery"`) — nothing is queued.**
See [`hold`](#hold--the-character-is-physically-stuck-and-the-client-cannot-free-it-724).

---

## `combat`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/combat/target` | `{"id":<spawn_id>}` | Target a spawn + auto-consider it. |
| `POST /v1/combat/target/name` | `{"name":"a rat"}` | Target a mob by fuzzy name. **Returns JSON** with [`matched`](#matched--which-entity-a-name-actually-resolved-to) — always check it before acting on the target. |
| `POST /v1/combat/attack` | — | Enable auto-attack. Swings at the **current target and only** that target — the client never re-picks it for you (#1109), so a target you set stays set until you change it or the spawn goes away. |
| `DELETE /v1/combat/attack` | — | Disable auto-attack. |
| `POST /v1/combat/consider` | `{"id":N}` (default current target) | Consider a spawn (difficulty tier + faction attitude). Result: `target_con`/`target_attitude`/`target_level` on `/observe/debug` if the spawn IS the current target, always `last_consider` regardless — see [Consider results](#consider-results). |
| `POST /v1/combat/cast` | `{"gem":0-8}` \| `{"spell_id":N,"target_id":M?}` \| `{"item_slot":N}` | Cast a memorized gem, a memorized spell by id, or an item clicky (on target, else current, else self). `gem`/`spell_id`/`item_slot` are **mutually exclusive** — two or more in one body is a `400`, never a silent pick (#952/#956, see [Mutually exclusive request forms](#mutually-exclusive-request-forms-952956)). `target_id` composes with all three. |
| `POST /v1/combat/memorize` | `{"spell_id":N,"gem":0-8}` | Memorize a known spell into a gem. |
| `POST /v1/combat/scribe` | `{"spell_id":N,"slot":B?}` | Scribe a spell scroll into the spellbook. |

---

## `interact`

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/interact/hail` | `{"name":"NPC"}` \| `{}` | Say "Hail, <name>" so an NPC fires its hail/quest script (nearest if no name). |
| `POST /v1/interact/say` | `{"text":"..."}` | Say arbitrary text on Say (quest keyword follow-ups). |
| `POST /v1/interact/loot` | `{"id":N}` \| `{"name":"..."}` \| `{}` | Loot a corpse (specific id, fuzzy name, or nearest). `id` and `name` are **mutually exclusive** — both in one body is a `400` (#952/#956). A spawn counts as a corpse if EITHER its name contains "corpse" OR the `dead`-roster (#1117, see `/observe/entities?labeled=1`) already says `true` for it — the second check closes the window right after a kill where the entity is already dead but the server has not yet renamed its key, so looting does not 404 "no corpse matching" for a kill you just landed. |
| `POST /v1/interact/give` | `{"npc":"Name","from":N}` | Hand inventory slot N to an NPC (quest turn-in trade flow). |
| `POST /v1/interact/click_door` | `{"door_id":N}` \| `{"name":"DOOR1"}` | Click a door (server-authoritative open). `door_id` and `name` are **mutually exclusive** — both in one body is a `400`, answered before the roster is consulted (#952/#956). |
| `POST /v1/interact/dialogue` | `{"index":N}` \| `{"text":"..."}` | Click one of `GET /v1/observe/dialogue`'s choices. `index` and `text` are **mutually exclusive** — both in one body is a `400`, answered before the "no dialogue choices available" `409` (#952/#956). |
| `POST /v1/interact/sit` | — | Sit (regen). |
| `POST /v1/interact/stand` | — | Stand. |

---

## `quests`

| Route | Body | Description |
|-------|------|-------------|
| `GET /v1/quests/log` | — | The native EQ Task journal (server-pushed) — active tasks only. Each objective carries `activity_id` + a `state` discriminator: `known` adds `activity_type`/`target`/`description`/`done_count`/`goal_count`/`optional`; `locked` (the server has not unlocked it — `???` in the native client) adds only `optional` and **omits the count keys entirely** rather than serving zeros; `undecodable` adds `reason`. See #889. |
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
| `POST /v1/inventory/move` | `{"from":N,"to":M}` | Move/equip/unequip an item between slots (0-21 worn, 22+ general (eight slots), 30 cursor, 251+ bag). Reads live under `GET /v1/observe/inventory`. |

---

## `events` — the async event feed

The bus an agent polls for "what just happened, as soon as it happened". Every event includes
`{id, category, kind, directed, from, text}`; action events also carry `request_id`, `result`, and `reason`:

- `id` — **1-based** monotonic cursor. Pass the response's `last_id` as your next `?since=`.
- `category` — top-level bucket: `chat` | `combat` | `navigate` | `system` | `action`.
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

`attacked` is edge-triggered off a 6s "recently swung at us" window, so it means *"this spawn was
not hitting you a moment ago"* rather than *"you have never been hit by this spawn"*. A lull longer
than that window — kiting, a root/mez, the mob switching to your pet — re-arms it, and the same
spawn fires `attacked` again when it resumes. Before #1109 that window was only pruned while
auto-attack was ON, so with it off a spawn fired `attacked` at most once per zone; the prune is
unconditional now, which makes the event honest in both states rather than only one.

---

## `chat` — send on the inter-agent channels

(The *incoming* side is the read-only `events` feed above.)

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/chat/tell` | `{"to":"Name","text":"..."}` | Directed whisper (chan 7). The recipient sees a `directed` chat event. |
| `POST /v1/chat/ooc` | `{"text":"..."}` | OOC broadcast (chan 5). |
| `POST /v1/chat/shout` | `{"text":"..."}` | Zone-wide shout (chan 3). |
| `POST /v1/chat/group` | `{"text":"..."}` | Group-channel message (chan 2). |

---

## `lifecycle`

| Route | Description |
|-------|-------------|
| `POST /v1/lifecycle/camp` | Toggle a camp (start, or cancel one in progress). A completed camp shuts the client down cleanly with no linkdead. |
| `POST /v1/lifecycle/exit` | Camp out (idempotent `Start`), then cleanly shut the process down (~30s). **Deliberately not gated on a live session** — tearing a zombie session down is what it is for. When `net_thread_dead` is non-null no camp can be sent (nothing is left to drain it), so the 200 body says so instead of promising a camp-out and relays the thread's own reason verbatim. Which body you get depends on WHICH state the thread is in, because the three are not interchangeable: a lost session (panic / fatal error / unexpected return) warns that the server declares a **linkdead** drop on its own timer — this shutdown inherits that, it does not cause it; a shutdown already under way says so and reports that this request changes nothing; and `--testzone`, where the thread was never started, says there is no server session at all and nothing is left linkdead. The first and third also say the process will exit via the 45s watchdog rather than in ~30s; the already-shutting-down one does not, because that process exits through its own main loop instead. The teardown proceeds in every case (#890). **Residual:** a net thread that is merely *wedged* has not ended, so `net_thread_dead` is still `null` and this endpoint still answers with the camp-out body even though nothing will drain that camp — cross-check `snapshot_age_ms`. |
| `POST /v1/lifecycle/respawn` | Revive a slain character at its bind point. On death the client now HOLDS the character dead (no auto-respawn) so an agent can inspect `dead`/`killed_by` in `/v1/observe/debug` and recover its corpse; this releases it. No-op (still 200) if not currently dead. (#284) |

---

## Notes

- **Queued mailbox actions return `202` with a `request_id`**. Read `/v1/events/action` for the
  correlated result. An `unconfirmed` result is unknown and must not be treated as success or a
  reason to retry blindly. See [Admission and action outcomes](#admission-and-action-outcomes-347).
- **Async travel**: movement commands return immediately; poll `GET /v1/observe/debug` (or watch
  for a `zone` event) to know when movement / a zone-in completed.
- **Coordinates**: server convention is `x=east, y=north, z=up`. Brewall map coords negate x/y.
- See `docs/autonomous-play.md` for end-to-end play recipes.

### Admission and action outcomes (#347)

Mutation routes in `combat`, `interact`, `merchant`, `inventory`, `quests`, `group`, `guild`,
`trainer`, `pet`, and `chat` return the following JSON when a mailbox command is accepted:

```json
{"request_id":42,"status":"accepted","message":"queued; outcome not yet confirmed"}
```

The status is **202 Accepted**. This replaces the previous queued `200` and optimistic text.
Validation errors and occupied-mailbox `409` responses still queue nothing and have no request ID.
Read-only routes and no-op responses do not create actions. Camera and lifecycle retain their
existing contracts.

`/merchant/buy`, `/merchant/open`, `/interact/give`, `/combat/cast`, and `/combat/target/name`
already await a real outcome. They keep their existing status and receipt fields, add
`request_id`, and also emit an action event. A cast's existing HTTP `200` can mean fizzled or
interrupted; check `landed` and `status`, not the HTTP status alone. `/combat/target/name`'s `200`
carries its `matched{}` disclosure (#513) straight through, unlike the other mutation routes above
whose synchronous `200`/`202` is replaced by the generic accepted receipt. Closing the HTTP
connection does not discard the result receiver: a later confirmation or refusal is still
published to the event feed.

Poll `/v1/events/action?since=<last_id>&wait=30` for correlated events:

```json
{"id":123,"category":"action","kind":"interact.loot","directed":true,"from":"",
 "text":"processed_without_server_confirmation","request_id":42,
 "result":"unconfirmed","reason":"processed_without_server_confirmation"}
```

`kind` is the route below `/v1/`, with slashes replaced by dots. Results are:

| Result | Meaning |
|--------|---------|
| `confirmed` | An existing awaited result path observed a positive server outcome. |
| `refused` | A known execution-time precondition failed or the awaited action reported failure. |
| `unconfirmed` | The client cannot establish the outcome: the command lacks a correlated reply path, the server was silent, or tracking expired. |

Each accepted request produces at most one terminal **tracking** result. Door clicks, loot,
memorize/unmemorize and group accept stay open after the packet is sent and settle on the server's
echo: `door_moved`, `corpse_opened` (or `refused` with the server's reason, or `unconfirmed` /
`loot_open_timeout`), `spell_memorized` / `spell_unmemorized`, `group_joined`. A silent server
leaves them to the 30-second `outcome_timeout`. Commands without a correlated server-result path
(for example scribing a scroll, which has no identifying echo) report `unconfirmed` after
drainage, even when the game action may later succeed. Admission and packet submission never manufacture confirmation. An
independent watchdog expires accepted tracking records after 30 seconds even if the network loop
stops. **Timeout does not remove a queued command**, so it can still execute later. Re-check game
state before deciding whether to retry. Partial HTTP request bodies are read with the usual size
limit before work is detached; abandoning a partial body does not submit an action.

The event feed retains 200 events across all categories. `dropped` counts lost cursor positions,
including gaps before game events reached the shared feed; filtered-out events are not counted as
lost. Re-check state if loss is reported. Request IDs and event cursors belong to one client process:
a restart requires resetting the cursor, and a process exit can prevent an outcome from being delivered.

**`404` — rejected at the door.** The client refuses a request its own published snapshot already
contradicts, instead of accepting it into a mailbox. Nothing is queued and nothing is sent:

| Route | Checked against | Refused when |
|-------|-----------------|--------------|
| `POST /v1/combat/target`, `POST /v1/combat/consider` | `GET /v1/observe/entities` | no spawn with that id is in the zone |
| `POST /v1/inventory/move` | `GET /v1/observe/inventory` | `from` holds no item |
| `POST /v1/interact/give` | `GET /v1/observe/inventory` | `from` holds no item |
| `POST /v1/merchant/sell` | `GET /v1/observe/inventory` | `slot` holds no item |
| `POST /v1/combat/scribe` | `GET /v1/observe/inventory` | `from` is supplied and holds no item |
| `POST /v1/interact/read` | `GET /v1/observe/inventory` | `slot` holds no item (`409` if it holds something unreadable) |
| `POST /v1/interact/click_door` | `GET /v1/observe/doors` | no door in the roster matches the `door_id` **or** the `name` — both forms are looked up, neither is taken on trust |

`click_door` is the one row where a `404` **never** means "contradicted". The roster is not a
closed set at any size: it grows as further door records arrive, and zoning empties it. So a
populated roster establishes only that no door it holds *right now* matches — not that the door
does not exist — and an empty roster does not even distinguish a zone with no doors from a zone
whose door records have not arrived, *or have arrived and not yet been published*. Records are now
published as they arrive during **both** zone-entry paths' own handshake drains, not only after them
— the re-zone handshake since #937/#1016, and a session's very first zone-in (which runs through a
separate login state machine) since #1022. On both paths the "arrived but unpublished" window is now
bounded by one drain **pass**, not by the length of the zone-in: each zone-entry drain applies every
packet it drained and then publishes once, at the end of the pass (the gameplay drain republishes
after every applied packet instead). That is smaller, not zero, and this document does not convert
it to milliseconds because nothing here measures a pass's duration. Read this `404` as *unknown*,
never as *disproved*. (`503` would claim the session is not live and `409`
would claim something is pending; neither is true, so the code stays `404` and the body carries the
distinction.)

**There is no observable that says the roster is complete**, so "re-check once the zone has
finished loading" is advice this API cannot underwrite for doors specifically: `zone_assets.state`
reaching `ready` is about geometry, and door records arrive as separate server packets on no
schedule this client publishes. An earlier revision of this paragraph said a populated roster
*did* mean contradicted; that reinstated exactly the over-read the `404` body was rewritten to
remove, and it contradicted both that body and the handler's own rustdoc.

The snapshot these are checked against is the one the network thread publishes. It is refreshed on
every inbound packet, **and** — since #347 — immediately after the client mirrors a change the server
applies without echoing it (an item move: EQEmu sends no `OP_MoveItem` back for a move you asked
for). So a `/v1/inventory/move` that has already been drained is visible to the next door check
without waiting for a packet to arrive. It is still a snapshot: it cannot know about a change only
the server has made and not yet told the client about.

**`409` — the slot is busy.** Each action endpoint queues into a single-slot mailbox the net thread
drains in its next pass. That pass runs on the network loop's ~10 ms cadence. A second request arriving
inside that window used to **replace** the first, and both callers were told `200` — one of the two
actions simply never happened and nothing
said so. It is now refused: the pending request is kept untouched and the second caller gets `409`
with a body ending `(it was NOT queued)`. A `409` is definitive — that request did not happen, so
retrying it after the drain is safe and cannot double-fire.

This applies to every verb under `/v1/combat`, `/v1/interact`, `/v1/inventory`, `/v1/merchant`,
`/v1/trainer`, `/v1/pet`, `/v1/group`, `/v1/quests` and `/v1/guild`. Three families are deliberately
exempt because a second request there is a *retarget*, not a lost action, and is already observable:
`/v1/move/*` (a latched wish is simply replaced),
`/v1/lifecycle/*` (`camp` is a toggle; `exit` must be able to override an in-progress camp), and
`/v1/social/*` (a superseded reply channel closes and answers `503`). `/v1/chat/*` is an unbounded
FIFO and never dropped anything.

---

### `target_cleared_reason` — why `target_id` is null (#1007)

- **`player.target_cleared_reason`** (`string | null`) — why `target_id` is currently `null`. It is
  stamped by the same funnel every real clear goes through (`GameState::clear_target`), and reset to
  `null` the moment a new target is set, so it always names the CURRENT null, never a stale one:

  | Value | Meaning |
  |-------|---------|
  | `null` | Either a target is currently set, or none has ever been cleared this session. |
  | `"target_despawned"` | The spawn that was targeted was removed from the world (died and derezzed, or otherwise despawned). Without this field, `auto_attack: true` with `target_id: null` after a kill looks identical to a bug — `auto_attack` is a separate, unrelated toggle that this clear does not touch, so it can legitimately stay `true` with nothing to swing at. |
  | `"zone_changed"` | Cleared by the ordinary zone-transition reset — the old target's id/hp/con were only ever valid in the zone you left. |
  | `"server_cleared"` | The server sent `OP_TargetCommand` with its "clear target" sentinel (corpse-locate, Sense Undead/Summoned/Animal, a merc-hire clear, or a GM/script force-clear) — nothing the client did caused this. |

### `target_dead` — is the CURRENT target a corpse? (#1117 follow-up)

**`player.target_dead`** (`bool | null`) — same gating as `target_con`/`target_level` above: `null`
while nothing is targeted, otherwise read straight off the targeted spawn's live `dead` field — the
same per-entity flag `GET /v1/observe/entities?labeled=1` publishes as `dead` and
`POST /v1/combat/target/name` publishes as `matched.dead` (#1117).
Exists so an agent can tell its OWN current target just died from this one read, without
cross-referencing `entities?labeled=1` by name or issuing a mutating request just to read
`matched.dead` back. **Flag, not a refusal** — a corpse target is still a valid target, this field
just tells the truth about it, same agent-honesty precedent as `player.dead` on this same payload
(#284/#406).

### `levitating` — three-valued levitate buff state, NOT a gravity reading (#598)

`player.levitating` reports whether the self-player currently has **Levitate** up (SPA 57 — gravity
off, the character free-floats instead of falling and holds altitude with no input). It is
**three-valued**, and the distinction is load-bearing for the agent-honesty invariant:

| Value | Meaning |
|-------|---------|
| `true`  | Levitating. `player.pos[2]` — the up component of the served position array `[east, north, up]` — is a height the character will **not** fall from, and the controller applies no gravity. |
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

### `buffs` — the full active-buff list (#1127)

`player.buffs` is an array of every currently-occupied buff slot on the self-player, whatever the
buff does — not just the levitate-only reading `levitating` above is derived from. Each entry:

```json
{"slot": 3, "spell_id": 1234, "duration_ticks": 42}
```

| Field | Meaning |
|-------|---------|
| `slot` | The buff slot id (stable per-buff while it's active; a fade/refresh keeps or reuses it per the server's own bookkeeping). |
| `spell_id` | The active spell's id — look it up against `spells_us.txt` for a name/effect, the same table `levitating` cross-references for SPA 57. |
| `duration_ticks` | Ticks remaining, straight off the wire, **signed**. A "tick" is EQ's spell-duration unit (~6 real seconds); this is not converted to seconds. A negative value is not a countdown — it's EQEmu's own sentinel encoding: `-1000` (`PERMANENT_BUFF_DURATION`) means the buff is permanent, not "4 billion ticks left" (which is what you'd get if this were misread as unsigned). |

The array is sorted by `slot` and is **empty, not omitted**, when nothing is active — the key is
always present. It is fed by the same two wire opcodes `levitating` is (`OP_Buff` for one slot at a
time, `OP_BuffCreate` for adds and full zone-in/resync snapshots), so both fields always agree about
which slots are occupied; `buffs` just keeps the raw spell id and duration instead of collapsing
everything down to a single levitate bool.

### `mana_cost` / `cast_time_ms` / `recast_time_ms` per gem — resource cost/cooldown (#1127)

`GET /v1/observe/spells` reports, for each of the 9 memorized gem slots, the spell's mana cost,
cast time, and recast (reuse) delay — read straight from the client's loaded `spells_us.txt` (the
same table [`levitating`](#levitating--three-valued-levitate-buff-state-not-a-gravity-reading-598)
cross-references SPA 57 against), not inferred or guessed:

```json
{"gem": 0, "spell_id": 300, "name": "Lightning Bolt", "mana_cost": 75, "cast_time_ms": 3000, "recast_time_ms": 2500}
```

| Field | Meaning |
|-------|---------|
| `mana_cost` | Mana required to cast, **signed** — a handful of spells (mana-granting clicks) carry a negative cost. |
| `cast_time_ms` | Cast time in milliseconds. |
| `recast_time_ms` | Minimum delay, in milliseconds, before this same spell can be cast again. `0` for most spells (no per-spell cooldown beyond the ordinary cast/recovery time). |

All three are **`null` together**, alongside `name: null`, for an empty gem (`spell_id: null`) or
for a memorized `spell_id` the loaded spell table has no row for (missing/truncated
`spells_us.txt`) — never a fabricated `0`, since `0` is itself a real, meaningful cost/delay for
plenty of spells and would be indistinguishable from "no data" if used as the not-known sentinel.
An agent must check for `null`, not assume `0` means "free and instant".

These are the client's own static numbers, not a live countdown: `recast_time_ms` is the *rule*
(how long the cooldown lasts once triggered), not *how much of it is left right now* — the client
does not currently track a live per-spell reuse timer, only this data from `spells_us.txt`.

### `hp_verified` — is the `hp` in this payload the server's? (#1005)

`player.hp_verified` says whether `hp`, `hp_max` and `hp_pct` in the **same payload** are what the
server last reported, or a number the client worked out for itself.

| Value | Meaning |
|-------|---------|
| `true`  | The three vitals are exactly the figures carried by the most recent self `OP_HPUpdate` — the only server message that carries both current and maximum HP — and nothing has written them since. |
| `false` | At least one of them is **client-side arithmetic**. It is usually close, it is *not* a reading, and it can be wildly wrong. |

The key is **always present**, never omitted, so an absent key can never be mistaken for "verified".

**Why this field exists.** The client used to apply each hit to your HP locally, on purpose
(eqoxide#55), so the reading moved per-hit instead of pinning at the last server value. Measured
live: one `#damage` command produced **two** damage lines, the client subtracted both, and this
endpoint published `hp: 0` for a character the server was holding at **214/441** — for up to
**2.477 s**, with `dead: false` in all 27,527 samples and no `OP_Death` packet, reproduced 2 of 2.
Every field in that response was well-formed and plausible, and nothing in it distinguished the
fabricated zero from server truth. An agent deciding whether to flee, heal or engage had no channel
that could tell.

**That arithmetic is gone.** `OP_Damage` no longer moves your published HP at all, and neither does
client-computed fall damage. The subtraction was never buying accuracy it could keep: EQEmu queues
the authoritative `SendHPUpdate(true)` for a client *before* it builds the `OP_Damage` packet for
the same hit, on the same reliable stream, so the real figure has already arrived by the time the
client could estimate one. The rule the client now follows is: compute damage only where the
protocol *requires* it to be reported (the `OP_ENV_DAMAGE` fall report, which the server has no way
to derive on its own), and never apply that number to published state.

So `hp` is the server's figure in the ordinary case. This flag covers the **residue** — the handful
of paths that still have to write your HP from an inference, because the fields must hold
something.

**Every writer that leaves it `false`:**

* the `OP_Death` zeroing of `hp` — the *death* is authoritative and `dead` reports it, but the
  *number* zero is the client's inference, not a reading;
* the bind-respawn "real EQ revives at full HP" assumption (eqoxide#68);
* the `OP_PlayerProfile` HP seed (eqoxide#19). Its `hp` **is** server-sent, but the profile carries
  no maximum at all, so `hp_max` is seeded equal to it and `hp_pct` then reads `100` for a character
  that zoned in wounded. Expect `hp_verified: false` from zone-in until the first `OP_HPUpdate`.

It governs **`target_hp_pct`** as well whenever you are self-targeted (F1) — but not for the reason
this page used to give. `target_hp_pct` does **not** resolve from `hp_pct` on read. For an ordinary
mob target the published figure follows that entity's own health. For the F1 self-target there is no
entity to follow — your own character is not in the entity list — so what you get is a stored
snapshot the client re-seeds at a few specific moments: when you select a target, when you clear it,
and on each HP write for whichever spawn you currently have targeted. The estimate reaches it
because the estimate path is one of those moments.

**Known gap — eqoxide#1033 (open).** The two writers that set your own HP *raw* rather than through
that path — the `OP_Death` zeroing and the `OP_PlayerProfile` seed — do not re-seed the snapshot. So
a self-targeted character that dies can publish `hp: 0`, `hp_pct: 0`, `dead: true` beside a stale
`target_hp_pct: 100` in one payload. `hp_verified` reads `false` in both of those states, so nothing
here is server truth being faked — but the two figures do contradict each other inside a single
response. Until #1033 is fixed: for **your own** health read `hp` / `hp_pct`, not `target_hp_pct`,
even when self-targeted.

The flag is deliberately **conservative**: it under-claims rather than over-claims. `false` does not
mean the number is wrong, only that the client cannot vouch for it. The outcome #1005 rules out is
the other direction — a `200` carrying a client-derived figure that reads as server truth.

**How to use it.** If a decision turns on an exact HP figure and `hp_verified` is `false`, wait for
the next `OP_HPUpdate` to reconcile — the flag flips to `true` the moment one lands *and is
recognised as yours*.

That qualifier is not pedantry, and the wait is **not bounded**. Recognising an update as yours is a
comparison against your own spawn id, and eqoxide#1006 — open, and explicitly an unverified reading
of the source rather than a measurement — records a way that comparison could silently never match.
If it turns out to be real for your character, no amount of waiting flips the flag: the client would
go on publishing its last known figure with `hp_verified: false` for the life of that spawn, with no
error and no log line to distinguish it from "your HP simply has not changed". So treat "the next
update flips it" as the expected case, not a guarantee. If your HP has demonstrably moved and the
flag has not flipped, stop waiting: treat the figure as unverifiable rather than blocking on it.

**What `true` does and does not promise.** It means the vitals match the last self `OP_HPUpdate` —
not that one arrived recently. Self-HP is **change-gated** at the server: it queues an update only
when your current HP actually moves, and the 2 s `hpupdate_timer` re-checks that same gate rather
than acting as a heartbeat. Measured: an idle window of **204.5 s at full HP produced zero self HP
updates**, and when HP was moving the observed cadence was the **~6 s regen tic**, not 2 s. So there
is no 2 s bound on anything here; the interval between updates is however long it takes your HP to
change next.

Two consequences worth knowing:

* If the client's number is ever wrong, the correction is not on a timer — it waits for the server's
  HP to move. A measured fall-damage divergence persisted **11.3 s** for exactly this reason: the
  server had already sent its one update, so from its point of view nothing had changed.
* The wire values are `current_hp - itembonuses.HP` and `max_hp - itembonuses.HP`, and the gate
  tests `current_hp`. So equipping or removing an item carrying `+HP` shifts both published numbers
  while `current_hp` itself does not change, and no update is sent until your HP next moves.
  `hp_verified` still reads `true` there, correctly: the client received those figures from the
  server and nothing local has touched them. That is a freshness gap, not an honesty gap — and one
  a local estimate could not have closed either, since the client does not know the item bonus.
  (This second case is a reading of the server source, not something observed.)

**Fall damage in the combat log.** The `Fell Nu — reported N fall damage to the server` line reports
what the client **asked for**, not what you took. The server may scale that number by the
environment-damage modifier, then by the spell/item/AA `ReduceFallDamage` bonuses, then by a rule
multiplier, and apply the result — or refuse it. The refusals are not uniform, in either the amount
or whether you hear about it:

| server branch | HP change | update sent? |
|---|---|---|
| normal | scaled damage applied | yes, immediately |
| GM, invulnerable, invulnerable to environmental damage, still loading | **−1** | no — only on the next 2 s poll, and only because that −1 passed the change gate |
| standing in liquid, tutorial and load zones | **none** | **no — ever**, because no HP ever changed |

So do not treat a fall as a promise that a fresh `hp` is about to arrive. The `hp` that follows the
next `OP_HPUpdate` is the outcome; the log line is only the request. Measured: the server answered
`Your GM status protects you from 160 points of Falling (Type 252) damage` and applied **1**, while
the old line announced `Fell 39u — 160 fall damage` (#1029). That measured run exercised the GM
branch; the invulnerability branches remain unexercised.

### `endurance` / `endurance_max` / `endurance_pct` / `endurance_verified` (#1127)

`player.endurance`, `player.endurance_max` and `player.endurance_pct` are the character's current
endurance, its maximum, and the derived percentage — the same shape as `mana`/`mana_max`/
`mana_pct`, but sourced differently.

`mana_max` is a **high-water-mark**: nothing on the wire carries a real mana maximum, so the client
infers it as the largest current-mana value it has ever seen (seeded from a rested caster's
`OP_PlayerProfile` at zone-in, then only ever grown by `OP_ManaChange`). Endurance does not have
that problem. `OP_EnduranceUpdate` — sent by the server alongside `OP_ManaChange` on every mana- or
endurance-changing event, for every class — carries **both** current and max endurance directly, so
`endurance_max` is the server's own figure, not an inference.

`player.endurance_verified` reads `true` once at least one real `OP_EnduranceUpdate` has been seen,
and `false` before that — the same honesty contract as
[`hp_verified`](#hp_verified--is-the-hp-in-this-payload-the-servers-1005): an untouched client
reports `endurance: 0`, `endurance_max: 0`, `endurance_verified: false` rather than a plausible-
looking figure nobody sent. The key is **always present**.

`OP_ManaChange` also carries a `stamina` field alongside its mana update — the same current
endurance value, delivered more often (any mana-or-endurance change, not just endurance changes).
The client applies it too, as a cheaper trickle update to `endurance`/`endurance_pct` — but it never
lowers `endurance_max` below a value `OP_EnduranceUpdate` has already confirmed, since that field
carries no max of its own.

### `hold` — the character is physically stuck and the client cannot free it (#724)

`player.hold` is `null` for a healthy character — **including one that is simply standing still** —
and non-null only while the movement controller has stopped the body and has no way to resume.

It exists because those two states were indistinguishable through this API. `pos` is correct in
both. No other served field distinguishes them, so a character that was summoned into a rock and is
standing there produced **no observable at all** — every movement command returned `200`, nothing
moved, and every other field read normal. (Those commands no longer answer `200` under an
`embedded_no_recovery` hold — they return `409 "status":"held"`, described later in this section
(#884) — but they did when this field was introduced, which is why it exists.)

```jsonc
"hold": {
  "reason":    "embedded_no_recovery",   // or "underworld_no_recovery"
  "held_secs": 12.4,                     // controller frame time, this unbroken hold
  "detail":    "…what is true and what you can do about it…"
}
```

| `reason` | What is true | Can the character move? |
|----------|--------------|-------------------------|
| `embedded_no_recovery` | The body **cannot be placed**: geometry pierces its footprint **or** there is no floor within 199 u below its feet. The push-out search found nowhere it can legally stand, there is no recovery position to fall back to (a position discontinuity — a GM summon, a large server correction — supersedes that history, #724), and the zone-wide last-resort search found nowhere either. | **No.** Physics is frozen: the controller's step returns before it reads driver input, so no wish of any shape moves the body. Since #884 the movement endpoints **refuse** (`409`, `"status":"held"`) rather than accept — before #884 they answered `200` and produced no motion. |
| `underworld_no_recovery` | The body fell to the zone's **underworld floor** and the client is holding it there rather than let it drop out of the world (#150), with no recovery position to restore. It is hanging: not falling, not landing, not grounded. | Horizontally, yes — but there is probably nothing under it. |

⚠️ **`embedded_no_recovery` does not mean geometry is inside the body.** It is the client's
`is_embedded` predicate, which is a disjunction: pierced footprint *or* an empty column. #845's live
casualty was measured (against the zone's own baked collision) to be the second — zero triangles
over the column, nearest ground 133 u away — while this table and the `detail` string both said
"embedded in world geometry". Nothing in the API distinguishes the two halves; if you need to know
which, look at the geometry, not at this field.

**Since #845 the client no longer only reports this state — it also tries to leave it.** When both
the push-out and the recovery ring come up empty, the client searches the zone (out to
`RESCUE_RADII`'s max, 512 u, retried about once a second) for anywhere a body could legally stand,
and relocates itself there.

⚠️ **This does not mean the hold you are looking at is about to clear. It means the opposite.**
Both directions were measured and both run the other way round:

- **A succeeding search never publishes this field at all.** The relocation happens inside the
  physics step and returns *before* the hold is raised, so `player.hold` stays `null` throughout —
  measured at **0 held frames out of 300** in a zone the search can solve. That is a property of
  how the app swaps `self.collision` (only two sites in `app.rs`, both passing through a `None`
  window that clears any hold — pinned by `the_frames_that_do_not_step_still_clear_the_hold`), not
  a guarantee the controller makes by itself: a hold raised before a swap to solvable geometry does
  produce a `Some(..) → null` transition.
- **A hold that *is* published does not clear on its own.** In a zone whose geometry does not
  change, the once-a-second retry keeps failing for the same reason — measured at **1800 frames /
  60 s, raised at frame 14 and never cleared**, with the body never moving a unit.

So a published `embedded_no_recovery` is the signature of a search that **failed**, and it still
takes something external to end it. (The contrast, same probe: truncate the search's reach so the
solvable zone above becomes unsolvable and it reports **286 held frames of 300**.) A `hold` going
non-null → null means the underlying *condition* ended — a GM moved the body, the character zoned,
or geometry finished loading in underneath it — which is what it has always meant.

This applies to `embedded_no_recovery` only. `underworld_no_recovery` was left alone deliberately:
it does not freeze lateral movement, so `/v1/move/manual` may still walk the character out over a
floor above the underworld, and #724 holds that body where the server put it on purpose. If it has
nowhere lateral to go, it still needs a GM.

#### Detecting a client-side relocation — `client_relocations` + `last_relocation` (#925)

The #845 search above can move the body up to `RESCUE_RADII`'s max, 512 u, with **no driver
request and no server correction** behind it. Two `player` fields make that jump observable — one to detect it, one to
attribute it. Both are **always present**: `0` and `null` until the first relocation.

```jsonc
"client_relocations": 2,                 // session total; NOT zone-scoped
"last_relocation": {
  "to_east":  111.5,                     // where the client put you — same frame/datum as player.pos
  "to_north": 222.25,
  "to_up":    -7.5,
  "distance": 84.0,                      // full 3-D size of the pos jump (NOT the log's horizontal-only figure)
  "detail":   "…the CLIENT moved the body itself…"
}
```

- **`player.client_relocations`** — a **session-monotonic** counter, the sibling of
  `server_corrections`, incremented once each time the #845 search relocates the body. **Poll it
  and watch for it to change** — that is the detect step. It is **not** zone-scoped: it survives
  zone changes so an infrequent poller still sees that a relocation happened.
- **`player.last_relocation`** — the detail of the **most recent** relocation, or `null`. It is
  **zone-scoped**: `GameState::begin_zone_in` clears it, because `to_east/to_north/to_up` name a
  point in the zone you just left. `distance` is the **full 3-D** straight-line move from the
  body's pre-placement position — deliberately not the horizontal-only number the `warn` log
  prints, because it is the size of the `pos` delta you will actually observe. Read this to
  attribute a jump: which `pos` you should now be at, and how big the discontinuity was.

The two fields that look like they might already serve still do not, and now you can see why:

- **`player.hold`** stays `null` through a *succeeding* relocation (it is raised only when the
  search finds nowhere — see above), so a non-null `last_relocation` and a null `hold` together
  mean "the client got you unstuck".
- **`player.server_corrections`** does not advance — correctly, this was not a server correction.
  Its one incrementer is the network layer's `OP_ClientUpdate` handler, gated on a >12 u
  server-vs-client horizontal delta; an unchanged value carries no information about client-side
  motion. `last_relocation` non-null with `server_corrections` unchanged is the signature to match.

The relocation is still logged at `warn` — `controller RELOCATED [#845]: … moved N u to …` — for
an operator; the `N` there is the horizontal-only distance, not `last_relocation.distance`.

**A persistent hold still needs an outside push, and an ordinary character has no client-API way to
produce one.** Every movement endpoint (`/v1/move/manual`, `/v1/move/jump`) depends on the physics step an `embedded_no_recovery` hold
has frozen; thirteen such calls were measured live on #845 and moved the body zero units. What did
work, each on the first frame, was a GM `#goto`, `#summon` or `#zone` — including one issued by the
held character itself through `POST /v1/interact/say`, since the hold freezes physics and not the
command channel. All three need GM status.

**Since #884 those five endpoints say so instead of answering `200`.** While
`player.hold.reason == "embedded_no_recovery"` each of them returns **`409 Conflict`** with

```jsonc
{
  "status":  "held",                    // machine token — match on this, not on the prose
  "hold":    { "reason": "embedded_no_recovery", "held_secs": 41.7, "detail": "…" },
  "message": "…"
}
```

and **nothing is queued**: no manual-move wish is set — the refusal is decided before any of
that, the same shape as the `dead` gate above. The embedded `hold` object is the same one
`/v1/observe/debug` serves under `player.hold`, so a caller that only ever sees the `409` still gets
the full reason, `held_secs` and `detail` without a second request.

`underworld_no_recovery` is deliberately **not** refused, for the reason above — lateral wishes still
reach the body there, and `/v1/move/manual` walking out is that state's only client-API exit, so
refusing it would be the same lie in the opposite direction.

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

**The key is always present in `GET /v1/observe/debug`'s `player` object** (never omitted), so an
agent that greps that response for `hold` and finds nothing knows it is talking to a client too old
to report the state, rather than concluding all is well. This is a claim about *that one route*, not
about the API in general — no other endpoint carries the field, and there is no bare `GET /v1/observe`
or `/v1/observe/state` route to carry it (#817 shipped the `player.insert("hold", …)` in
`observe::get_debug` that makes the key reachable at all; before it, `player.hold` was computed,
mirrored into `GameState` every tick, and covered by tests, and still reached no response body).
And it does not latch. On every **rendered** frame the controller recomputes it from scratch, so it
disappears the frame the body is freed. On the frames that render but do not **step** it is cleared
explicitly instead: for the whole ~10 s of a zone's asset load there is no collision to step against,
and a zone-in clears the mirrored copy, so a hold never survives into a zone the character has left.
If the render loop goes idle it stops recomputing altogether —
but a held body cannot be *freed* without a stepped frame either, so idling cannot manufacture a
false hold; what it freezes is `held_secs`, and the paragraph above tells you how to detect that.
(#724 round-3 review, N1 — this used to say "recomputes it from scratch every frame", which is not
true of the load, zone-in or idle paths.)

**Why an idle render loop cannot free your body behind `hold`'s back — and the two places the client
withdraws it on purpose (#846).** The obvious attack is the one the table above
tells you to use: a GM `#summon`. It arrives on the *network* thread, and the network thread is not
the one that recomputes the hold. It still cannot free your body behind the hold's back — which is
what would make the field a confident falsehood rather than merely an old one. The movement
controller is owned by the render thread, in a crate the network code cannot even name, so the
network side has no way to move your body: all it can do is hand the new coordinates over and wait,
and the render frame that picks them up is the frame that clears the hold and recomputes. Adopting
the summon and dropping the hold are the same frame, and the network side of that boundary is
property-tested (`no_net_tick_can_free_or_manufacture_a_hold_846`, plus
`the_hold_mirror_tracks_the_render_thread_over_time_846` for the case where the render thread
publishes a *different* answer, withdrawal included).

Two places the client does clear `hold` from the network side, both deliberately and both in the
"say less" direction — you may briefly get `null` where a hold is still in force, never a hold where
there is none:

- **A zone change.** A hold describes collision geometry, and a zone-in drops it. `hold` goes `null`
  for the whole load rather than reporting a wedge in a zone you have left.
- **The tick a server reposition is handed to the render thread.** For that tick `pos` is already
  the server's new coordinates, so the client withdraws `hold` rather than pair fresh coordinates
  with the predicament you were just lifted out of. (An earlier draft of this section described that
  mismatch as a one-tick window you could poll inside. That was measured against a server that
  asserts the correction once; against one that re-asserts it every tick while the render loop
  idles, it recurred indefinitely. It is withdrawn now instead of bounded.)

One honest caveat remains: nothing above bounds how *long* the render loop may idle before it picks
a correction up. It is woken by any inbound state change, including the summon itself, which works
out to tens of milliseconds — but that number is **derived from the loop's own idle-poll and frame
constants, not observed on a running client**, and the wake itself is reached by no test in this
repo (measured: forcing the condition dead leaves the whole suite green and unchanged). Treat it as
a latency expectation, not a promise. If it matters to you, the `held_secs`-against-your-own-clock
check above is what distinguishes a frozen controller from a live one.

### `afloat_stall` — this swimmer is being asked to swim and is going nowhere (#776/#801)

`player.afloat_stall`, in the `player` object of **`GET /v1/observe/debug`**, is `null` for every
ordinary character, **including every ordinary swimmer**, and non-null only while the body is
*afloat*, *being wished at horizontally*, and *not getting anywhere*.

It exists because a genuinely trapped swimmer had **no observable at all**. A body afloat in water
never enters the client's depenetration net, so nothing the API served distinguished it from a
swimmer making perfect progress: `pos` barely moved and no stall counter was published for a
manually-driven or directly-wished swimmer. Every served field said "swimming normally", which is the
silent-wrong-answer class this project ranks above crashes. (The controller's own internal state did
know — `in_water` true, `on_ground` false — but neither of those is a key in any response body, then
or now, so an agent could not read them. Naming them as if an agent could is the mistake #810's
round-2 review caught one paragraph further down.)

```jsonc
"afloat_stall": {
  "secs":                 4.8,        // controller frame time, this unbroken stall
  "anchor_east":         -161.2,      // the point it has failed to get away from…
  "anchor_north":         842.7,      // …same frame and datum as this object's own `pos`
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
| `secs` | How long the stall has been continuously in force, in **controller frame time** as of the last stepped frame — the same clock and the same caveat as `hold.held_secs`, both documented above. |
| `anchor_*` | The position the window opened at: the point the body has failed to get more than `progress_threshold` away from, in any direction. Same coordinate frame and FOOT datum as **`player.pos`** in this same response, which is the array `[east, north, up]` — so `anchor_east - pos[0]`, `anchor_north - pos[1]`, `anchor_up - pos[2]` are the drift on each axis, differenceable directly. (Position is served as that one array; there are no `pos_east`/`pos_north`/`pos_up` keys — those are internal field names, not part of this contract.) |
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
makes it true is one `player.insert("afloat_stall", …)` in `crates/eqoxide-agent-ops/src/observe.rs`'s
`get_debug`, and one test that reads it back out of a real response through the real router. It does
not latch: the controller recomputes it on every stepped frame, the render
thread republishes it on every rendered frame in the same statement that republishes `hold`, the
frames that render without stepping clear it explicitly, and a zone-in clears the mirrored copy — so
a stall can never survive into a zone the character has left, which matters more here than for
`hold` because a stall names an *anchor position* in the departed zone's coordinates. If the render
loop goes idle it stops recomputing, but a stalled body cannot be *freed* without a stepped frame
either, so idling cannot manufacture a stall; what it freezes is `secs`. Detect that directly: poll
`afloat_stall.secs` twice and compare the delta against your own wall clock — a `secs` that advances
by much less than the elapsed time is a render loop that has gone idle, not a stall that is being
re-earned. Since #817, `player.hold.held_secs` is served the same way and detects a stalled render
loop by the identical procedure (poll twice, compare the delta against your own wall clock). Before
#817, `hold` was not served by any handler at all, so this paragraph used to warn that "detectable
exactly the way `held_secs` is" pointed at a field you could not actually poll; that caveat no
longer applies.

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

`zone_exits` has a **second, unrelated 503** that this readiness verdict does not cover — see
[Zone exits: `[]` means exactly one thing (#803)](#zone-exits--means-exactly-one-thing-803).

**Endpoints that are deliberately NOT gated**, because they do not read zone geometry or collision:
`/v1/observe/doors`, `/v1/observe/entities` and `/v1/observe/zone_entrances` (all three are
server-pushed lists, not derived from the collision grid), and `/v1/move/manual` and `/v1/move/jump`
(they drive the controller directly and make no routing claim — though with no collision loaded the
character is moving through a world the client has not built, so prefer waiting for `ready`).

**Ungated is not the same as honest during a load, and it applies to each of those three observe
endpoints.** The zone-entry handshake empties all three server-pushed rosters — the door roster
(#891/#934), and the entity roster and `zone_points` (#1010/#1063) — before it asks for the new
zone's records, and, like `zone_exits` before #803, an empty body from an emptied roster is the same
bytes as the true answer "this zone has none of these". A session's FIRST zone-in reaches the same
empty by a different route: those rosters start empty, so there is nothing to clear and the same
ambiguity holds without any clear having run. Unlike `zone_exits`, there is no `ready` gate to wait
on for any of them, because they are server-pushed
lists, not derivations from loaded geometry (see "Endpoints that are deliberately NOT gated" above).
The three are enumerated in band, with the client's own limit stated alongside them, on
`GET /v1/observe/debug` — see
[`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073).

The rest of this subsection is about **doors specifically**, whose publishing side has its own
history; `entities` and `zone_entrances` are covered in the linked section above.

**For doors, one of the two things that used to produce that ambiguous `[]` is now addressed on both
zone-entry paths (#937/#1016, then #1022).** Each zone-entry path parses and applies door records on its OWN
drain loop, separate from the post-handshake gameplay drain that normally republishes them. Until
#937 neither of those drains called the publish step at all, so a door applied mid-zone-in stayed
unpublished — readable nowhere — until the first drain pass *after* the zone-in finished, no matter
how early it had actually arrived.

* **Re-zone** (`run_zone_entry_handshake`, reached only from inside the main gameplay loop): its
  drain calls the publish step itself since #937/#1016, on the same pass that applies a door record.
  Failing the handshake (timeout) clears the roster rather than leaving a partial one behind next to
  `zone_in_failed: true` (#1016 review B4) — the same confident-falsehood shape #934 removed
  elsewhere.
* **A session's first zone-in** (the earlier, separate login handshake): it applied `OP_SpawnDoor` to
  game state the same way but had no path to publish it at all, so the original #937 shape stood for
  however long that handshake took. Since #1022 it publishes through the identical projection on the
  identical per-pass gate, and any failed login attempt clears the roster it published rather than
  leaving a partial one readable across the retry backoff and the next attempt.

**What remains for doors is #939 on both paths, plus a one-drain-pass residue of #937's shape on
both.** A zone-in that has not yet delivered its *first* door record reads as an empty, doorless
zone (#939, unchanged) — that ambiguity has no packet to publish and nothing this client can do about it. And
because each zone-entry drain applies every packet of a pass and publishes once at the end of it, a
record applied earlier in the same pass is still unpublished for the remainder of that pass. That
residue is bounded by one drain iteration rather than by a whole zone-in, but it is not zero, and
this document states it in drain passes rather than milliseconds because nothing here measures a
pass's duration. Re-listing is the usual recourse; if you need to positively confirm a record has
landed, re-list.
Treat a persistent `[]` as *not yet known*,
not as *this zone has no doors*, until the zone-in has otherwise finished.

### `server_pushed_rosters` — what an empty roster means (#939, #1073)

Top-level on `GET /v1/observe/debug`, **always present** — in every state, unlike the
`null`-when-healthy fields around it. It is not a fault report that appears when something breaks;
it is a standing statement of a limit that never goes away, and an absent key would be
indistinguishable from an older client that never had it.

Three `observe` endpoints serve a roster the client fills from records the *server* pushes, rather
than deriving anything itself:

| Roster key | Endpoint | Filled from |
|---|---|---|
| `doors` | `GET /v1/observe/doors` | `OP_SpawnDoor` records |
| `entities` | `GET /v1/observe/entities` | spawn packets |
| `zone_entrances` | `GET /v1/observe/zone_entrances` (alias `/zone_points`) | `OP_SendZonepoints` |

For all three, an empty body has two readings — *this zone has none* and *the records have not
arrived yet* — and nothing published told them apart. The zone-entry handshake clears all three
before it asks for the new zone's records (doors #891/#934; the entity roster and `zone_points`
#1010/#1063), which is the right call — serving the zone you just left is the worse lie — but it
makes the ambiguous empty the NORMAL reading for the length of a zone-in rather than a rare one.

```jsonc
"server_pushed_rosters": {
  "rosters": {
    "doors":          { "endpoint": "/v1/observe/doors",          "held": 0, "complete": null },
    "entities":       { "endpoint": "/v1/observe/entities",       "held": 0, "complete": null },
    "zone_entrances": { "endpoint": "/v1/observe/zone_entrances", "held": 0, "complete": null }
  },
  "no_completeness_signal": "…the sentence every `complete` above is null for…"
}
```

- **`held`** — how many entries that roster held when this payload was built, counted off the same
  shared slot the endpoint serves. For `entities` it is the **deduped** count (#471) — the same
  projection the endpoint returns, not the raw table behind it. It is a reading, not a prediction:
  two separate HTTP requests can straddle a publish, so `held` does not promise what a later `GET`
  on that endpoint will return.
- **`complete`** — **`null` on every roster, always.** No code path in this client sets it to `true`
  or `false`, and this document is not promising one will. It is spelled as a three-valued field so
  that "unknown" is something you *read* rather than infer from an absent key, and so that a roster
  which one day acquires a real signal has somewhere to put it without a shape change.
- **`no_completeness_signal`** — the same statement in words, published in band so an agent gets it
  off the payload it is already reading rather than out of this file, which it never sees.

**Why `complete` is `null` today.** #939 asked for one of three things, in cost order: (1) a
nullable reason naming *why* the roster is empty; (2) a
server-advertised total to compare holdings against; or, failing both, (3) an explicit "no
completeness signal exists" field, so the endpoint discloses its own limit in band rather than only
in prose. **This is (3).** (1) is not built here because this client retains no record of whether
the packet that would separate "not sent yet" from "none exist" has arrived, so it has no reason to
name. (2) is not built here: this client compares its holdings against no total. For
`zone_entrances` the server does advertise one — EQEmu's `Client::SendZonePoints()` writes
`zp->count` (`eqemu:zone/client.cpp@5dc093fe5:6959`) and the RoF2 patch copies it onto the wire
(`eqemu:common/patches/rof2.cpp@5dc093fe5:3632`) — and since #1094 `apply_zone_points` **does** read it, but **only
to bound its parse**, never to judge the roster. It could not be compared naively even if something
wanted to: the #136 sentinel filter drops entries from inside the counted range, so `held` and the
count legitimately differ. Turning it into a completeness observable is #939's scope, not this
disclosure's.

**`zone_assets.state` is not this signal.** It gates loaded terrain and collision — geometry — not
which packets have arrived. That is why none of these three endpoints is gated on it (see
"Endpoints that are deliberately NOT gated" above), and why `ready` is not something to wait for
before believing a roster.

**The limit binds the populated case too.** A non-empty roster is not a closed set either: it grows
as further records arrive. "Not among the entries held right now" is the strongest claim available
at any size, which is why `POST /v1/interact/click_door` answers a lookup miss with `404` *unknown*
rather than *disproved*. Do not read a populated list as a complete one.

**What this does not do.** It publishes no new fact about the world — it publishes the client's own
limit, so an agent can stop mistaking an empty roster for a reading of an empty zone. If you need to
positively confirm a record landed rather than infer it, the recourse is unchanged: re-list.

**The enumeration is checked against the routes, not asserted.** `observe.rs`'s
`server_pushed_roster_completeness_939_1073` tests drive every endpoint the payload names and
compare its entry count against the `held` the same payload published, and assert the key set is
exactly these three — so the list cannot drift from what the router serves, and no member can be
dropped or crowned.

### Zone exits: `[]` means exactly one thing (#803)

`GET /v1/observe/zone_exits` answers out of the zone's **region map** (`maps/water/<zone>.wtr`),
whose baked zone-line regions *are* the exits. That file is separate from the terrain the
[`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579) verdict
tracks, and it can fail on its own: absent, unreadable, not region data, a format version this build
cannot read, or truncated. Until #803 all of those were discarded and the endpoint served **`[]` with
`200 OK`** — the same bytes as the true, common answer "this zone has no zone lines". Exits are the
only way out of a zone, so a failed *file read* read as a fact about the world: sealed in.

Now:

* **`200` with `[]`** means, and only means, *this zone's region map loaded and contains no
  zone-line regions.* It is a real reading of the world.
* **`503 {"error": "zone_region_data_unavailable", "reason": …, "detail": …, "message": …}`** means
  the question could not be answered. Unlike `zone_assets_not_ready`, **this does not clear by
  polling** — the asset is missing or unusable, not loading. Re-sync or re-bake the zone's assets.
  `reason` is the machine-readable cause below, `detail` is that cause rendered with its specifics
  (e.g. the declared node count), and `message` is prose for a human reading the log.

| `reason` | What happened |
|---|---|
| `region_data_missing` | No `.wtr` for this zone. |
| `region_data_unreadable` | The file exists but could not be read (permissions, a bad mount, a directory in its place). |
| `region_data_not_region_data` | Present, but not a region map (wrong magic, or shorter than the header). |
| `region_data_unsupported_version` | A `.wtr` format version this build cannot read. |
| `region_data_truncated` | The header declares more BSP nodes than the file carries. |

**A sixth `reason` exists in the code and is NOT in that table on purpose: `region_data_not_attached`**
(#821 review round 2, B3 — a previous revision listed it as an outcome you might receive). It is the
state a freshly built collision grid is in before any region data is handed to it. **No release
build can serve it**: the client has exactly one production construction of a collision grid
(`build_zone_collision` in `src/app.rs`), which builds and attaches in a single call, and the only
reason-free way to write the slot (`Collision::set_water`) is `#[cfg(any(test, feature =
"test-fixtures"))]` and so does not exist in a release binary. It is listed here only so that an
agent that somehow *does* receive it knows what it means: **a client bug, not an asset problem** —
re-syncing assets will not help; file it. (This is an argument from enumerating every construction
site, not a type-level guarantee — nothing stops a future non-test grid from reaching a reader
un-attached.)

### `matched` — which entity a name actually resolved to (#513)

Name-resolving endpoints (`POST /v1/combat/target/name`) return a **`matched`** object naming the
entity they actually picked. **Check it.** A name is fuzzy-matched against the live spawn table, and before #513 these
endpoints returned only coordinates / a bare success, so a resolution that silently landed on a
*different* spawn than you meant was undetectable (a live near-miss routed `"a_rodent020"` to a
distant NPC named `Astaed_Wemor`).

```jsonc
POST /v1/combat/target/name {"name": "a gnoll"}
{
  "matched": {
    "id": 437,           // spawn id actually routed to / targeted
    "name": "a gnoll",   // its canonical (cleaned) name
    "quality": "exact",  // "exact" | "fuzzy"
    "candidates": 5,     // how many spawns matched EQUALLY well
    "distance": 1163.2,  // units from you; OMITTED when not known
    "dead": false        // #1117: true if this spawn is a corpse
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
| `dead` | (#1117) `true` if the matched spawn is a corpse. **Flag, not a refusal**: a corpse still resolves and is still targeted/routed-to — it may be inspectable/lootable later — this field just tells you the truth instead of a corpse looking like a live mob of the same name. `false` for a genuinely-alive entity; it is also the defensive default this client would fall back to if a resolved spawn were ever missing from the internal dead roster, but that cannot happen in production — positions, ids, poses and the dead flag are published together in one atomic step, so a name this client can resolve always has a dead-flag entry too. Same agent-honesty precedent as `player.dead` on `GET /v1/observe/debug` (#284/#406). |

A name that
matches nothing at all — not even fuzzily — is an honest **404**, never a distant wrong match.

> **Content type:** `/v1/combat/target/name` returns `application/json`. The `move` routes
> (`/manual`, `/jump`) return `text/plain`.

### Mutually exclusive request forms (#952/#956)

This is a rule that covers the whole API. Several routes
accept **more than one way to name the same argument**, and each of them used to resolve the choice
with a precedence chain that never looked at the losers. Every request struct carries
`serde(deny_unknown_fields)`, so a *misspelled* field is rejected. That is exactly what makes a
*declared but unreached* field quiet: it is not unknown, so it parses, and the request is answered
`200`. That is the failure this rule removes: the strongest signal the API can send ("your
instruction was understood") for an instruction that was thrown away.

> **#971 closed the last two gaps.** Every request struct is covered: all 20 of this crate's request structs carry
> `deny_unknown_fields`, so an unrecognized query key is **refused** rather than dropped. The empty
> exception set is asserted by `every_deserialize_request_struct_is_classified`, and the `20` on this page
> against that module's constants by
> `docs_http_api_md_may_not_disagree_with_this_modules_struct_counts`. That test also holds the `7`
> and the `25` fixed relative to that figure, but neither is derived from the code — re-derive them
> with the `grep` commands under "How far the guard actually reaches" below.

**The rule.** Where two forms name *the same thing* in different notations (`{map_x,map_y}` and
`{x,y,z}` for one point), precedence applies and the loser is **reported** in `ignored_fields`.
Where two forms name *different things* — two spawns, two commands, two spells — no reading of the
request makes both correct, so the request is **refused** with a `400` that names every conflicting
field it received, in the order the route declares them. Nothing is queued and no state changes.

```jsonc
POST /v1/pet/command {"command": 2, "name": "sit"}
400 "conflicting pet action: this request supplied command, name — mutually exclusive forms of one
     argument, of which exactly one may be sent. They denote different things, so there is no reading
     under which both were meant and nothing was chosen for you: this request was REFUSED, nothing
     was queued, and no state changed. Re-send with a single form."
```

That example is not a paraphrase: a test unwraps it out of this file and asserts the router produces
exactly it, so it cannot drift from the code.

**The groups, and what each used to answer.** Every row was driven through the real router on the
pre-fix tree by one guard test, and the "was" column is that run's response — with one marked
exception where the probe body happened to fail for an unrelated reason and a second body was needed
to expose the discard.

| Route | Mutually exclusive fields | Was |
|-------|---------------------------|-----|
| `POST /v1/pet/command` | `command`, `name` | `200 pet command 2 queued` |
| `POST /v1/trainer/open` | `name`, `trainer` | `200 opening training with Beta (spawn_id=102)` |
| `POST /v1/social/friends` | `add`, `remove` | `200 added Alpha` (the removal never happened) |
| `POST /v1/interact/loot` | `id`, `name` | `200 looting <the id's corpse>` |
| `POST /v1/interact/click_door` | `door_id`, `name` | `200 clicking door 7` |
| `POST /v1/interact/dialogue` | `index`, `text` | `200 clicking '<choice at index>'` |
| `POST /v1/combat/cast` | `gem`, `spell_id`, `item_slot` | see below † |

† `/cast` is the marked exception. The guard's probe body (`gem` + `spell_id` + `item_slot`) drew a
`400 no item at slot 23` — a refusal, but for an unrelated missing-item reason, naming none of the
conflicting fields. The discard itself was measured with a second body: `{"gem":0,"spell_id":202}`
with spell 202 memorized in a *different* gem answered `409 spell gem 0 is empty — memorize a spell
into it first`. `spell_id` lost the precedence chain silently and the caller was told its problem
was an empty gem. That case is pinned by `cast_gem_beside_spell_id_names_both_rather_than_reporting_the_wrong_gem_empty`.

**Where the check sits.** After the session/liveness gates, and **before** anything that inspects
world state. A body carrying two forms is malformed whatever the world looks like, so answering a
state error first (`409 no dialogue choices available`, `404 no door matching …`) would send an
agent to fix the world when the thing to fix is its own request.

**What is NOT in a group.** Fields that compose rather than compete are unaffected and still combine
freely — `target_id` beside any of `/cast`'s three forms, `allow_pending` beside `/frame`'s camera
params, `z` beside `{map_x,map_y}`.

**How far the guard actually reaches** — stated precisely, because the natural summary of it is
broader than the thing it does:

* A test reads the HTTP crate's `src/` directory and requires every `Deserialize` request struct in
  it — all **20** — to be classified as exclusive or composable. So a **new request struct** cannot
  be added without that decision being made out loud.
* A **new field on an existing struct** is a different case, and only **5** of the 20 are protected
  against it: the seven handlers that destructure their body exhaustively (`let CommandBody { command,
  name } = &b;`, no `..`), where adding a field is a compile error until someone places it. On the
  other 15 structs, a new field that no code path reads would compile and ship — which is the
  original #952 shape. Re-derive both figures:
  `grep -rn '^#\[derive(.*Deserialize' crates/eqoxide-agent-ops/src/ | wc -l` and
  `grep -rnE '^\s*let [A-Za-z]+Body \{[^}]*\} = ' crates/eqoxide-agent-ops/src/*.rs`.
* The 25 `COMPOSABLE` rows carry a one-line reason each ("all three passed to `request_mem_spell`").
  Those reasons are **read off the source and written down, not executed** — the guard verifies that
  each struct has been *classified*, not that a classification is *correct*. A struct wrongly filed
  as composable is invisible to it.

**Migration note:** a caller that batched two forms into one request now gets a `400` where it
previously got a `200` that performed only one of them. Three routes change answer for bodies that
were previously accepted:

* `POST /v1/social/friends` `{"add":…,"remove":…}` — previously `200` performing only the add.
* `POST /v1/trainer/open` `{"name":"Alpha","trainer":"Beta"}` — previously `200` opening Beta.
* `POST /v1/interact/dialogue` `{"index":0,"text":"bind"}` — previously `200` clicking the index.

Note the presence rule differs by route. `/social/friends` treats a **blank** name as not supplied, so
`{"add":"","remove":"Beta"}` is still an ordinary removal and is *not* refused (a test seeds Beta and
asserts `200 removed Beta`). `/trainer/open` and `/interact/dialogue` key on the field being present
at all, so a blank loser now refuses too — both driven through the router:
`POST /v1/trainer/open {"name":"","trainer":"Beta"}` and
`POST /v1/interact/dialogue {"index":0,"text":""}` each answer `400 conflicting …`. Send one request
per form.

### Zone-cross degradations you can *detect* (#713)

Two zone-cross conditions used to be reported only as English in the message log. Prose in a log is
not a machine-readable signal, so each now has a structured disclosure on
`GET /v1/observe/debug`, in the same shape as the other top-level disclosure objects there: an
object with a `reason` token and a human `detail`, or **`null` while there is nothing to disclose**.

| Field | Non-null means |
|-------|----------------|
| `zone_cross_stopped` | The automatic crossing that fires while you stand on zone-line geometry has **given up**: `{"reason":"cross_attempt_limit","region_index":I,"attempts":3,"detail":…}`. Nothing further will be attempted for as long as you stand there. |

**The attempt bound.** Standing on a zone line makes the client attempt a crossing every ~10 s
(the auto-cross cooldown). If the server denies it — or simply never answers — that used to repeat
**forever**, one server-side anomaly per attempt. The client now makes at most **3** attempts during
one continuous stand and then stops.

Three is a judgement, not a measurement: 1 is too few because a denial can be transient, the
interval is the 10 s cooldown so 3 spans ~twenty seconds of retrying, and an over-tight bound is cheap to
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

Before #646, only `GET /v1/observe/debug` (`snapshot_age_ms`, above) carried any freshness signal. Every other `/v1/observe/*` route served its
last-known state with no way for a driving agent to tell it was frozen — the motivating case: with
the `eq-net` thread dead, `GET /v1/observe/entities` kept returning `200` with a frozen entity map
and no marker of any kind (#634/#647).

**It is the SAME clock as `/debug`'s `snapshot_age_ms`, not a new one**: `OpState::health()`'s
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

`/debug` is unchanged by #646 — it already had its own freshness field (`snapshot_age_ms`).

### `net_thread_dead` — the frozen world's terminality (#634)

Top-level on `GET /v1/observe/debug`, beside `zone_assets`. `null` while the `eq-net` thread — the client's sole writer of world state, sole
drainer of command slots, sole stamper of the health clocks — is running. A **reason string** once it
has ended, for any reason:

| Cause | Reported |
|-------|----------|
| the thread PANICKED | `"the eq-net thread PANICKED (<message>) — …"` |
| a fatal error (login retries exhausted, server-rejected create) | `"the eq-net thread exited with a fatal error (<e>) — …"` |
| it returned with no shutdown requested | `"…returned WITHOUT a shutdown being requested — …"` |
| it returned while a shutdown WAS requested | `"…exited normally after a shutdown was requested — …"` |
| `--testzone` (offline renderer; no thread was ever started) | `"--testzone: the eq-net thread was never started …"` |

That fourth row is **not** the ordinary `/v1/lifecycle/exit` teardown, despite what an earlier
revision of this table said. On the ordinary teardown the gameplay loop calls
`perform_clean_shutdown` and then parks rather than returning, so the thread never unwinds and
this field stays `null` for the whole shutdown. Reaching that row needs the gameplay phase to
return for some other reason (a zone-transition failure, a server-side session drop) inside a
shutdown window (#890).

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

**`POST /v1/lifecycle/exit` is the deliberate exception among the commands that reach the server**,
because it must work on a dead session — tearing one down is its job. It answers `200`, and its body
describes the state the net thread is actually in rather than the healthy camp-out path: the camp
cannot be sent, so the shutdown cannot end the session with a clean camp-out (#890). (The other
guard-skipping route is not an exception to the sentence above, because it never reaches the
server: `/v1/social/friends`, which is documented client-local and sends no packet.)

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

---

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
  **Zone-scoped** (#883): `spawn_id` is a per-zone namespace, so a zone change clears this back to
  `null` — the same id in the new zone is ordinarily a different spawn at a different difficulty, and
  `ago_secs` (derived from `at` at read time) would otherwise keep counting normally without
  disclosing that the record predates the zone change. Consider again after zoning to repopulate it.

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
