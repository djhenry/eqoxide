# eqoxide agent ops

Reference for the agent ops eqoxide serves over the agent socket. eqoxide itself does not listen
on TCP: start it with `--agent-socket PATH` and run the
[harness-http](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar) wrapper against
that socket (`harness-http PATH [bind-addr]`, default `127.0.0.1:8090`). The wrapper serves these ops
over HTTP and forwards the ones it does not handle itself to eqoxide as `Call`s (see `agent-api.md`),
so every op below answers on the harness's address.

## Naming and grouping

Every op is named `<group>.<action>`, for example `observe.inventory` or `merchant.buy`. The
`Op` enum in the `eqoxide-agent-protocol` crate lists the ops and the arguments each takes, and a
client sends them as `{"id": N, "op": "<group>.<action>", "args": {...}}`. The harness maps its
HTTP routes onto these names. Where a table below says "400", "404", "409" or "503", the `Call`
answers `Invalid`, `NotFound`, `Conflict` or `Unavailable` (see `agent-api.md`).

Groups mirror the agent's mental model (and the `eqoxide_mcp` tool grouping):

| Group | Purpose |
|-------|---------|
| `observe`   | read-only world/player state |
| `combat`    | targeting, auto-attack, consider, spell scribe/memorize/cast |
| `interact`  | hail, say, loot, give (turn-in), doors, sit/stand |
| `quests`    | native EQ task journal (server-pushed), task offers/accept/decline/cancel |
| `merchant`  | open/close a vendor, list wares, buy, sell |
| `inventory` | inventory management actions |
| `events`    | read the async event feed (chat/combat/navigate/action/group/loot/merchant) |
| `chat`      | send messages on the inter-agent channels (tell/ooc/shout/group) |
| `lifecycle` | session control: camp / exit |

Not every group below is a `Call` op. The `chat`, `lifecycle`, `inventory` and `pet` groups and
`interact` sit/stand are **verbs** on the agent socket (`Chat::*`, `Lifecycle::*`, `Inventory::Move/Destroy/Consume`,
`Pet::Command`, `Interact::Sit/Stand`) and are not ops. Everything else is an op a client sends as
a `Call`. `agent-api.md` has the table of which is which. The implementation lives in
`crates/eqoxide-agent-plugin-host/src/ops/<group>.rs`, and `dispatch.rs` runs each `Op` variant.

---

## `observe` — read-only state

| Op | Description |
|-------|-------------|
| `observe.inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency, coin_verified, snapshot_age_ms}`. Slots are Titanium **wire** ids (DB general slots 23-30 are one lower on the wire, starting at 22). |
| `observe.messages[?kind=npc]` | Machine-readable message log (oldest→newest). `{count, messages, snapshot_age_ms}`; each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `observe.dialogue` | Pending NPC dialogue/quest choices `{count, choices:[{index, text}], snapshot_age_ms}`. |
| `observe.spells` | The 9 memorized gems `{gems:[{gem, spell_id, name, mana_cost, cast_time_ms, recast_time_ms}], snapshot_age_ms}` (empty gem, or an id our spell table has no row for → all of `name`/`mana_cost`/`cast_time_ms`/`recast_time_ms` null). See [`mana_cost` / `cast_time_ms` / `recast_time_ms`](#mana_cost--cast_time_ms--recast_time_ms-per-gem--resource-costcooldown-1127). |
| `observe.skills` | All skills with current trained value `{skills:[{id, name, value}], snapshot_age_ms}`; `value == 0` means untrained. |
| `observe.doors` | Current zone's doors — `{doors:[{door_id,name,x,y,z,heading,opentype,is_open}], snapshot_age_ms}`. **An empty `doors` does not mean "this zone has no doors"** — the roster is server-pushed and zone-in empties it, so an empty roster is "no record held", not "none exist"; see "Empty server-pushed rosters" below. |
| `observe.zone_entrances` | Zone entrance points received from the server (arrival side), plus a handful of client-synthesized entries read from the CURRENT zone's own map (the heuristic only ever recognizes a label naming North/South Qeynos or Qeynos2, but — measured — five zones' shipped map packs actually carry such a label: see [`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816) for the list and method). Served as `{zone_entrances:[…], snapshot_age_ms}`. **If those synthesized entries failed to load, this list is silently short**. Separately from that load gap, **an empty `zone_entrances` does not mean "this zone has no entrances"**: zone-in empties this list too (#1010/#1063) and the server-advertised entries refill on no schedule the client controls — see "Empty server-pushed rosters" below. |
| `observe.zone_exits` | Current zone's exits (the WLD zone-line regions you navigate toward — see [`zone_assets_not_ready`](#zone-asset-readiness-503) for its 503 gating). An entry with `"zone_id": null` is a REAL exit region whose baked index matches no advertised zone point (#683) — its destination is honestly unknown until the server resolves a crossing there. Crossing it works **only if this zone advertises no same-zone teleport point and server zone points are available** (received and not all filtered client-side) — the same #679 gate that keeps the client from firing a blind crossing off an intra-zone pad. In a gated zone (e.g. one with teleport pads), standing on a `zone_id: null` exit does NOT cross. **Each entry carries `"gated": true|false` (#713)** — it reports the **#679/#683 zone-level** unresolved-cross gate and nothing else, so you can see that refusal **before** walking there instead of reading about it in the message log after. **`gated` is a property of the zone and the entry, not of you**: your position is not an input to it. `gated` is only ever `true` for `zone_id: null` entries (an advertised destination is crossed directly and is never subject to the #679 gate); it **reports** the gate verdict and does not change it. **`gated: false` is not a promise the auto-cross will fire.** It says only that this gate is open — the stand-scoped #713 attempt bound can independently have stopped auto-crossing, and `zone_exits` never consults it, so a `gated: false` exit that never auto-crosses may have hit that bound. The message-log line ("auto-cross is disabled here", once per stand) is still emitted for callers that watch events. Served as `{zone_exits:[…], snapshot_age_ms}`. |
| `observe.item_text` | Text of the most recently read book/note `{text, snapshot_age_ms}` (`text: null` if none read this session). |
| `observe.who` | Server-wide `/who all` roster `{online:[{name, level, class, race, zone_id, guild, anon}], snapshot_age_ms}`. 503 if no response arrives in time. |

---

## `combat`

| Op | Body | Description |
|-------|------|-------------|
| `combat.target` | `{"id":<spawn_id>}` | Target a spawn + auto-consider it. |
| `combat.target/name` | `{"name":"a rat"}` | Target a mob by fuzzy name. **Returns JSON** with [`matched`](#matched--which-entity-a-name-actually-resolved-to) — always check it before acting on the target. |
| `combat.attack` | — | Enable auto-attack. Swings at the **current target and only** that target — the client never re-picks it for you (#1109), so a target you set stays set until you change it or the spawn goes away. |
| `DELETE combat.attack` | — | Disable auto-attack. |
| `combat.consider` | `{"id":N}` (default current target) | Consider a spawn (difficulty tier + faction attitude). The result arrives as the consider reply (difficulty tier + attitude). |
| `combat.cast` | `{"gem":0-8}` \| `{"spell_id":N,"target_id":M?}` \| `{"item_slot":N}` | Cast a memorized gem, a memorized spell by id, or an item clicky (on target, else current, else self). `gem`/`spell_id`/`item_slot` are **mutually exclusive** — two or more in one body is a `400`, never a silent pick (#952/#956, see [Mutually exclusive request forms](#mutually-exclusive-request-forms-952956)). `target_id` composes with all three. |
| `combat.memorize` | `{"spell_id":N,"gem":0-8}` | Memorize a known spell into a gem. |
| `combat.scribe` | `{"spell_id":N,"slot":B?}` | Scribe a spell scroll into the spellbook. |

---

## `interact`

| Op | Body | Description |
|-------|------|-------------|
| `interact.hail` | `{"name":"NPC"}` \| `{}` | Say "Hail, <name>" so an NPC fires its hail/quest script (nearest if no name). |
| `interact.say` | `{"text":"..."}` | Say arbitrary text on Say (quest keyword follow-ups). |
| `interact.loot` | `{"id":N}` \| `{"name":"..."}` \| `{}` | Loot a corpse (specific id, fuzzy name, or nearest). `id` and `name` are **mutually exclusive** — both in one body is a `400` (#952/#956). A spawn counts as a corpse if EITHER its name contains "corpse" OR the observed `dead` flag (#1117) already says `true` for it — the second check closes the window right after a kill where the entity is already dead but the server has not yet renamed its key, so looting does not 404 "no corpse matching" for a kill you just landed. |
| `interact.give` | `{"npc":"Name","from":N}` | Hand inventory slot N to an NPC (quest turn-in trade flow). |
| `interact.click_door` | `{"door_id":N}` \| `{"name":"DOOR1"}` | Click a door (server-authoritative open). `door_id` and `name` are **mutually exclusive** — both in one body is a `400`, answered before the roster is consulted (#952/#956). |
| `interact.dialogue` | `{"index":N}` \| `{"text":"..."}` | Click one of `observe.dialogue`'s choices. `index` and `text` are **mutually exclusive** — both in one body is a `400`, answered before the "no dialogue choices available" `409` (#952/#956). |
| `interact.sit` | — | Sit (regen). |
| `interact.stand` | — | Stand. |

---

## `quests`

| Op | Body | Description |
|-------|------|-------------|
| `quests.log` | — | The native EQ Task journal (server-pushed) — active tasks only. Each objective carries `activity_id` + a `state` discriminator: `known` adds `activity_type`/`target`/`description`/`done_count`/`goal_count`/`optional`; `locked` (the server has not unlocked it — `???` in the native client) adds only `optional` and **omits the count keys entirely** rather than serving zeros; `undecodable` adds `reason`. See #889. |
| `quests.completed` | — | Completed task history: `{task_id, title, completed_time}[]`. |
| `quests.offers` | — | Pending task offers from an open selector window: `{task_id, npc_id, title, description, has_rewards}[]`. |
| `quests.accept` | `{"task_id":N}` | Accept one offered task. |
| `quests.decline` | — | Decline all pending task offers. |
| `quests.cancel` | `{"task_id":N}` | Abandon an active task. |

---

## `merchant`

| Op | Body | Description |
|-------|------|-------------|
| `merchant.open` | `{"merchant":"Name"}` | Open a vendor window (OP_ShopRequest). Watch `list.open` for the result. |
| `merchant.close` | — | Close the merchant window. |
| `merchant.list` | — | The open merchant's wares `{open, merchant_id, count, items:[{merchant_slot,item_id,name,icon,price,quantity}]}`. |
| `merchant.buy` | `{"merchant":"Name","slot":N}` | Open the merchant and buy item slot N. |
| `merchant.sell` | `{"merchant":"Name","slot":N,"quantity":Q?}` | Sell player inventory slot N (qty default 1). |

---

## `inventory`

| Op | Body | Description |
|-------|------|-------------|
| `inventory.move` | `{"from":N,"to":M}` | Move/equip/unequip an item between slots (0-21 worn, 22+ general (eight slots), 30 cursor, 251+ bag). Reads live under `observe.inventory`. |

---

## `events` — the async event feed

The bus an agent polls for "what just happened, as soon as it happened". Every event includes
`{id, category, kind, directed, from, text}`; action events also carry `request_id`, `result`, and `reason`:

- `id` — **1-based** monotonic cursor. Pass the response's `last_id` as your next `?since=`.
- `category` — top-level bucket: `action` | `chat` | `combat` | `group` | `loot` | `merchant` | `navigate`.
- `kind` — sub-type within the category (e.g. chat→tell/ooc/shout/group/gmsay, navigate→zone,
  combat→slain/attacked).
- `directed` — concerns *you* specifically (a chat.tell to your name, a GM message, your own zone change
  or death).

| Op | Query | Description |
|-------|-------|-------------|
| `events.read` | `category`, `since`, `wait`, `directed` | `category` selects one bucket, e.g. `combat`, `navigate`, `chat`, and a category outside the list above is a 400. Omit it for all events. |

`wait` (seconds) long-polls up to ~30s for a matching event (loop it to "listen" without busy-polling);
`since` (an event id) returns only newer events; `directed: 1` filters to events addressed to you.

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

| Op | Body | Description |
|-------|------|-------------|
| `chat.tell` | `{"to":"Name","text":"..."}` | Directed whisper (chan 7). The recipient sees a `directed` chat event. |
| `chat.ooc` | `{"text":"..."}` | OOC broadcast (chan 5). |
| `chat.shout` | `{"text":"..."}` | Zone-wide shout (chan 3). |
| `chat.group` | `{"text":"..."}` | Group-channel message (chan 2). |

---

## `lifecycle`

| Op | Description |
|-------|-------------|
| `lifecycle.camp` | Toggle a camp (start, or cancel one in progress). A completed camp shuts the client down cleanly with no linkdead. |
| `lifecycle.exit` | Camp out (idempotent `Start`), then cleanly shut the process down (~30s). **Deliberately not gated on a live session** — tearing a zombie session down is what it is for. When `net_thread_dead` is non-null no camp can be sent (nothing is left to drain it), so the 200 body says so instead of promising a camp-out and relays the thread's own reason verbatim. Which body you get depends on WHICH state the thread is in, because the three are not interchangeable: a lost session (panic / fatal error / unexpected return) warns that the server declares a **linkdead** drop on its own timer — this shutdown inherits that, it does not cause it; a shutdown already under way says so and reports that this request changes nothing; and `--testzone`, where the thread was never started, says there is no server session at all and nothing is left linkdead. The first and third also say the process will exit via the 45s watchdog rather than in ~30s; the already-shutting-down one does not, because that process exits through its own main loop instead. The teardown proceeds in every case (#890). **Residual:** a net thread that is merely *wedged* has not ended, so `net_thread_dead` is still `null` and this endpoint still answers with the camp-out body even though nothing will drain that camp — cross-check `snapshot_age_ms`. |
| `lifecycle.respawn` | Revive a slain character at its bind point. On death the client now HOLDS the character dead (no auto-respawn) so an agent can notice the death and recover its corpse; this releases it. No-op (still 200) if not currently dead. (#284) |

---

## Notes

- **Queued mailbox actions return `202` with a `request_id`**. Read `events.action` for the
  correlated result. An `unconfirmed` result is unknown and must not be treated as success or a
  reason to retry blindly. See [Admission and action outcomes](#admission-and-action-outcomes-347).
- **Invites answer `202` for any name.** `group.invite` and `guild.invite` name a player who may be
  in another zone, so the client cannot tell a missing player from an absent one. The server's
  answer arrives in `events.action` and `events.group`.
- **Async travel**: movement commands return immediately; poll the observation (or watch
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
already await a real outcome. They answer with a `Receipt` or `Refused` that carries
`request_id`, and also emit an action event. A cast's existing `200` can mean fizzled or
interrupted; check `landed` and `status`, not the status alone. `/combat/target/name`'s `200`
carries its `matched{}` disclosure (#513) straight through, unlike the other mutation routes above
whose synchronous `200`/`202` is replaced by the generic accepted receipt. A caller that disconnects
does not discard the result receiver: a later confirmation or refusal is still
published to the event feed.

### Reply bodies

Every body is a type from the `eqoxide_agent_protocol::reply` module. The variant is not on the
wire, so a client reads a body with `CallReply::typed(&op)` and the `Op` that it sent.

| Status | Body |
|--------|------|
| `ok`, reads | the op's struct, for example `Inventory` for `observe.inventory` |
| `ok`, acknowledgement | a string |
| `ok` or `accepted`, tracked ops | `Receipt`, tagged by `status` as `accepted`, `unconfirmed`, `open`, `bought` or `given` |
| any refusal | a string, a `Refused` for awaited ops, or `Unavailable` for `observe.*` reads |

A `Receipt` always carries `request_id`. `unconfirmed` also names the `merchant`, `slot`, `npc` or
`from` that the call sent. A `Refused` body carries `status:"refused"`, `reason` and the
`request_id` of the action that it settled, and an `Unavailable` body carries `error`, `reason` and
`message`. A `409` for an occupied command slot is a string and has no `request_id`.

Poll `events.action?since=<last_id>&wait=30` for correlated events:

```json
{"id":123,"category":"action","kind":"interact.loot","directed":true,"from":"",
 "text":"processed_without_server_confirmation","request_id":42,
 "result":"unconfirmed","reason":"processed_without_server_confirmation"}
```

`kind` is the op name. Results are:

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
state before deciding whether to retry. Partial request bodies are read with the usual size
limit before work is detached; abandoning a partial body does not submit an action.

The event feed retains 200 events across all categories. `dropped` counts lost cursor positions,
including gaps before game events reached the shared feed; filtered-out events are not counted as
lost. Re-check state if loss is reported. Request IDs and event cursors belong to one client process:
a restart requires resetting the cursor, and a process exit can prevent an outcome from being delivered.

**`404` — rejected at the door.** The client refuses a request its own published snapshot already
contradicts, instead of accepting it into a mailbox. Nothing is queued and nothing is sent:

| Op | Checked against | Refused when |
|-------|-----------------|--------------|
| `combat.target`, `combat.consider` | the observed entities | no spawn with that id is in the zone |
| `inventory.move` | `observe.inventory` | `from` holds no item |
| `interact.give` | `observe.inventory` | `from` holds no item |
| `merchant.sell` | `observe.inventory` | `slot` holds no item |
| `combat.scribe` | `observe.inventory` | `from` is supplied and holds no item |
| `interact.read` | `observe.inventory` | `slot` holds no item (`409` if it holds something unreadable) |
| `interact.click_door` | `observe.doors` | no door in the roster matches the `door_id` **or** the `name` — both forms are looked up, neither is taken on trust |

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
finished loading" is advice this API cannot underwrite for doors specifically: zone-asset readiness
reaching `ready` is about geometry, and door records arrive as separate server packets on no
schedule this client publishes. An earlier revision of this paragraph said a populated roster
*did* mean contradicted; that reinstated exactly the over-read the `404` body was rewritten to
remove, and it contradicted both that body and the handler's own rustdoc.

The snapshot these are checked against is the one the network thread publishes. It is refreshed on
every inbound packet, **and** — since #347 — immediately after the client mirrors a change the server
applies without echoing it (an item move: EQEmu sends no `OP_MoveItem` back for a move you asked
for). So a `inventory.move` that has already been drained is visible to the next door check
without waiting for a packet to arrive. It is still a snapshot: it cannot know about a change only
the server has made and not yet told the client about.

**`409` — the slot is busy.** Each action endpoint queues into a single-slot mailbox the net thread
drains in its next pass. That pass runs on the network loop's ~10 ms cadence. A second request arriving
inside that window used to **replace** the first, and both callers were told `200` — one of the two
actions simply never happened and nothing
said so. It is now refused: the pending request is kept untouched and the second caller gets `409`
with a body ending `(it was NOT queued)`. A `409` is definitive — that request did not happen, so
retrying it after the drain is safe and cannot double-fire.

This applies to every op and verb in the `combat`, `interact`, `inventory`, `merchant`, `trainer`,
`pet`, `group`, `quests` and `guild` groups. Three families are deliberately
exempt because a second request there is a *retarget*, not a lost action, and is already observable:
movement (a latched wish is simply replaced),
`lifecycle` (`camp` is a toggle; `exit` must be able to override an in-progress camp), and
`social` (a superseded reply channel closes and answers `503`). `chat` is an unbounded
FIFO and never dropped anything.

---

### `mana_cost` / `cast_time_ms` / `recast_time_ms` per gem — resource cost/cooldown (#1127)

`observe.spells` reports, for each of the 9 memorized gem slots, the spell's mana cost,
cast time, and recast (reuse) delay — read straight from the client's loaded `spells_us.txt` (the
same table `levitating`
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

### Zone asset readiness (503)

The `observe` ops that describe the world (`zone_exits`, and the others that read the loaded terrain or
collision grid) answer **`503`** with `"error":"zone_assets_not_ready"` and a `reason` while the loaded zone
assets cannot describe the zone the character is in (still loading, stale from the previous zone, or
failed). The body carries a `zone_assets` object with the readiness `state`. Retry while it is loading;
a `failed` state will never become ready.

### Empty server-pushed rosters

`doors`, `zone_entrances` and similar rosters are pushed by the server and emptied on zone-in. An empty
list therefore means "no record held", not "none exist", and there is no observable that says the roster
is complete.

### Zone exits: `[]` means exactly one thing (#803)

`observe.zone_exits` answers out of the zone's **region map** (`maps/water/<zone>.wtr`),
whose baked zone-line regions *are* the exits. That file is separate from the terrain the
zone-asset readiness ([above](#zone-asset-readiness-503)) verdict
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

Name-resolving endpoints (`combat.target/name`) return a **`matched`** object naming the
entity they actually picked. **Check it.** A name is fuzzy-matched against the live spawn table, and before #513 these
endpoints returned only coordinates / a bare success, so a resolution that silently landed on a
*different* spawn than you meant was undetectable (a live near-miss routed `"a_rodent020"` to a
distant NPC named `Astaed_Wemor`).

```jsonc
combat.target/name {"name": "a gnoll"}
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
| `dead` | (#1117) `true` if the matched spawn is a corpse. **Flag, not a refusal**: a corpse still resolves and is still targeted/routed-to — it may be inspectable/lootable later — this field just tells you the truth instead of a corpse looking like a live mob of the same name. `false` for a genuinely-alive entity; it is also the defensive default this client would fall back to if a resolved spawn were ever missing from the internal dead roster, but that cannot happen in production — positions, ids, poses and the dead flag are published together in one atomic step, so a name this client can resolve always has a dead-flag entry too. Same agent-honesty precedent as the `dead` flag on the agent observation's `visible` entries (#284/#406). |

A name that
matches nothing at all — not even fuzzily — is an honest **404**, never a distant wrong match.

> **Content type:** `combat.target/name` returns `application/json`. The `move` routes
> (`/manual`, `/jump`) return `text/plain`.

### Mutually exclusive request forms (#952/#956)

This is a rule that covers the whole API. Several routes
accept **more than one way to name the same argument**, and each of them used to resolve the choice
with a precedence chain that never looked at the losers. Every request struct carries
`serde(deny_unknown_fields)`, so a *misspelled* field is rejected. That is exactly what makes a
*declared but unreached* field quiet: it is not unknown, so it parses, and the request is answered
`200`. That is the failure this rule removes: the strongest signal the API can send ("your
instruction was understood") for an instruction that was thrown away.

> **#971 closed the last two gaps.** Every request struct is covered: all of this crate's request structs carry
> `deny_unknown_fields`, so an unrecognized query key is **refused** rather than dropped. The empty
> exception set is asserted by `every_deserialize_request_struct_is_classified`.

**The rule.** Where two forms name *the same thing* in different notations (`{map_x,map_y}` and
`{x,y,z}` for one point), precedence applies and the loser is **reported** in `ignored_fields`.
Where two forms name *different things* — two spawns, two commands, two spells — no reading of the
request makes both correct, so the request is **refused** with a `400` that names every conflicting
field it received, in the order the route declares them. Nothing is queued and no state changes.

```jsonc
social.edit_friends {"add": "Alpha", "remove": "Beta"}
Invalid "conflicting friends-list edit: this request supplied add, remove — mutually exclusive forms of one
     argument, of which exactly one may be sent. They denote different things, so there is no reading
     under which both were meant and nothing was chosen for you: this request was REFUSED, nothing
     was queued, and no state changed. Re-send with a single form."
```

The same sentence, with the subject and fields changed, answers every group below.

**The groups, and what each used to answer.** Every row was driven through the real router on the
pre-fix tree by one guard test, and the "was" column is that run's response — with one marked
exception where the probe body happened to fail for an unrelated reason and a second body was needed
to expose the discard.

| Op | Mutually exclusive fields | Was |
|-------|---------------------------|-----|
| `trainer.open` | `name`, `trainer` | `200 opening training with Beta (spawn_id=102)` |
| `social.edit_friends` | `add`, `remove` | `200 added Alpha` (the removal never happened) |
| `interact.loot` | `id`, `name` | `200 looting <the id's corpse>` |
| `interact.click_door` | `door_id`, `name` | `200 clicking door 7` |
| `interact.dialogue` | `index`, `text` | `200 clicking '<choice at index>'` |

**Where the check sits.** After the session/liveness gates, and **before** anything that inspects
world state. A body carrying two forms is malformed whatever the world looks like, so answering a
state error first (`409 no dialogue choices available`, `404 no door matching …`) would send an
agent to fix the world when the thing to fix is its own request.

**What is NOT in a group.** Fields that compose rather than compete are unaffected and still combine
freely — `target_id` beside any of `/cast`'s three forms, `allow_pending` beside the other fields of its op,
`z` beside `{map_x,map_y}`.

**How far the guard actually reaches** — stated precisely, because the natural summary of it is
broader than the thing it does:

* A test reads the `eqoxide-agent-plugin-host` crate's `src/` directory and requires every `Deserialize` request struct in
  it — all **20** — to be classified as exclusive or composable. So a **new request struct** cannot
  be added without that decision being made out loud.
* A **new field on an existing struct** is a different case, and only **5** of the 20 are protected
  against it: the seven handlers that destructure their body exhaustively (`let CommandBody { command,
  name } = &b;`, no `..`), where adding a field is a compile error until someone places it. On the
  other 15 structs, a new field that no code path reads would compile and ship — which is the
  original #952 shape. Re-derive both figures:
  `grep -rn '^#\[derive(.*Deserialize' crates/eqoxide-agent-plugin-host/src/ops/ | wc -l` and
  `grep -rnE '^\s*let [A-Za-z]+Body \{[^}]*\} = ' crates/eqoxide-agent-plugin-host/src/ops/*.rs`.
* The 25 `COMPOSABLE` rows carry a one-line reason each ("all three passed to `request_mem_spell`").
  Those reasons are **read off the source and written down, not executed** — the guard verifies that
  each struct has been *classified*, not that a classification is *correct*. A struct wrongly filed
  as composable is invisible to it.

**Migration note:** a caller that batched two forms into one request now gets a `400` where it
previously got a `200` that performed only one of them. Three routes change answer for bodies that
were previously accepted:

* `social.edit_friends` `{"add":…,"remove":…}` — previously `200` performing only the add.
* `trainer.open` `{"name":"Alpha","trainer":"Beta"}` — previously `200` opening Beta.
* `interact.dialogue` `{"index":0,"text":"bind"}` — previously `200` clicking the index.

Note the presence rule differs by route. `/social/friends` treats a **blank** name as not supplied, so
`{"add":"","remove":"Beta"}` is still an ordinary removal and is *not* refused (a test seeds Beta and
asserts `200 removed Beta`). `/trainer/open` and `/interact/dialogue` key on the field being present
at all, so a blank loser now refuses too — both driven through the router:
`trainer.open {"name":"","trainer":"Beta"}` and
`interact.dialogue {"index":0,"text":""}` each answer `400 conflicting …`. Send one request
per form.

