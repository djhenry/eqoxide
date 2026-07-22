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
| `GET /v1/observe/debug` | Player (zone, race, class, level, pos `[east,north,up]`, heading ccw/cw, `currency`, server_corrections, vitals `hp_pct`/`hp`/`hp_max`/`mana_pct`/`xp_pct`, target `target_id`/`target_name`/`target_hp_pct`/`target_con`/`target_attitude`/`target_level`) + **navigation** (`nav_state`, `nav_reason`, `nav_goal_id`, `nav_goal`, `nav_blocked_by`, `nav_tier` — see [Navigation state](#navigation-state)) + **connection health** (`connected`, `link_age_ms`, `last_packet_age_ms`, `snapshot_age_ms`, `world_responsive`, `last_world_response_ms`, `send_failures`, `send_failures_unretried`, `last_send_error`, `last_send_error_age_ms` — see [Connection health](#connection-health)) + **`last_consider`** (spawn-scoped result of the most recent consider of ANY spawn, target or not — see [Consider results](#consider-results)) + camera state. |
| `GET /v1/observe/frame` | Current rendered frame as a PNG (`Content-Type: image/png`). **503 while the zone's assets are still loading** — see [`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579); `?allow_pending=1` opts past it. |
| `GET /v1/observe/entities[?labeled=1]` | Default: `{ "<name>": [x,y,z], ... }` for all known entities, with same-base-name + byte-identical-position duplicates collapsed (#471 — suspected server-side `spawn2` duplication; the model is untouched so each instance is still targetable by its full name). `?labeled=1` returns the richer `{count, entities:{"<name>":[x,y,z]}, deduped, duplicate_groups:[{position,names,kept}], note}` exposing which duplicates were collapsed. |
| `GET /v1/observe/inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency}`. Slots are Titanium **wire** ids (DB general slots 23-30 → wire 22-29). |
| `GET /v1/observe/messages[?kind=npc]` | Machine-readable message log (oldest→newest). Each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `GET /v1/observe/spells` | The 9 memorized gems `{gem, spell_id, name}` (empty = null). |
| `GET /v1/observe/doors` | Current zone's doors `{door_id,name,x,y,z,heading,opentype,is_open}`. |
| `GET /v1/observe/zone_points` | Zone exit points received from the server. |
| `GET /v1/observe/nav_debug` | The nav diagnostics snapshot navigation **publishes** (#608) — see [Nav diagnostics](#nav-diagnostics-get-v1observenav_debug--608). |

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
  thread drains each tick. The HTTP 200 means *queued*, not *done* — observe the result via
  `GET /v1/observe/*` or the `chat/events` feed.
- **Async travel**: `move/goto` / `move/zone_cross` return immediately; poll `GET /v1/observe/debug` (or watch
  for a `zone` event) to know when movement / a zone-in completed.
- **Coordinates**: server convention is `x=east, y=north, z=up`. Brewall map coords negate x/y.
- See `docs/autonomous-play.md` for end-to-end play recipes.

---

## Navigation state

`GET /v1/observe/debug` carries **`nav_state`** (what navigation is doing) and **`nav_reason`** (the
machine-readable *why*, `null` unless a state has one). Together they are how you find out whether a
`/v1/move/*` you fired actually worked — the 200 only means *queued*.

| `nav_state` | Meaning | `nav_reason` |
|-------------|---------|--------------|
| `pending` | A `/move/{goto,follow,zone_cross}` was **just accepted** and the walker has not ticked yet. Transient (≤ ~150 ms), then becomes `planning`/`navigating`/`following`. Its purpose is honesty: the instant a new request is accepted the state resets to `pending` (under a fresh `nav_goal_id`), so a read can **never** return the *previous* goal's terminal `arrived`/`no_path`/`blocked` as if it were the new request's outcome (#349). | — |
| `idle` | Nothing to do. | — |
| `planning` | A route is being computed on the pathfinding worker thread. The character stands still. Normally < 1 s. | — |
| `navigating` | Walking a **complete route to your goal**. | `goal_z_snapped` (see below) or — |
| `navigating_partial` | Walking a **partial** route: the search was cut short, so this is *not* a route to your goal — it's progress toward a frontier, and it will re-plan from the far end. Usually resolves to `navigating` or `arrived`. | `search_node_cap` |
| `following` | A `/follow` chase has caught up; holding near the leader, still latched. | — |
| `arrived` | Reached the goal. | `goal_z_snapped` (see below) or — |
| `no_path` | **DEFINITIVE: no route exists.** The planner searched to completion. Do not retry the same goal — pick another. | see below |
| `search_exhausted` | The planner **gave up**. This is **"I don't know", not "no"** — a route may well exist. Try a nearer waypoint. | `search_node_cap` |
| `blocked` | A route exists, but the walker **could not follow it** (wedged after 8 recovery attempts). Not a routing failure. | `walker_stalled`, `local_no_way_through`, `fall_would_be_lethal` |
| `zone_loading` | **This client has no model of the zone yet** — its terrain/collision are still loading, or their load failed (#579). No search was run and no route exists to report; the goal is kept and planned for real once the assets land. Read `zone_assets` (below) to tell *pending* from *terminally failed*. | `zone_assets_not_loaded` |

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
of the real zone. Only `ready` means the image shows the zone the character is in.

**Endpoints that are deliberately NOT gated**, because they do not read zone geometry or collision
and are honest during a load: `/v1/observe/doors` and `/v1/observe/zone_entrances` (both are
server-pushed lists, not derived from the collision grid), and `/v1/move/manual` and `/v1/move/jump`
(they drive the controller directly and make no routing claim — though with no collision loaded the
character is moving through a world the client has not built, so prefer waiting for `ready`).

`POST /v1/move/goto` still accepts the goal, but its response carries a non-null
**`zone_assets_pending`** note while the assets are missing, and `nav_state` reads `zone_loading`
until they land.

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

`GET /v1/observe/debug` carries two more top-level fields under `player`:

- **`nav_goal_id`** — a monotonically increasing counter, bumped every time a `POST /v1/move/{goto,follow,zone_cross,stop}` is accepted. It is **echoed in each of those POST's response bodies**: as a JSON `"goal_id": N` field on `/goto` and `/follow`, and as `[goal_id=N]` in the text body of `/stop` and `/zone_cross`. `nav_state`/`nav_reason` are the status *of this goal id* — never of an earlier one.
- **`nav_goal`** — that goal's `[x, y, z]` (server coords), or `null` for `idle`/`stop`, or for a `zone_cross` whose concrete zone-line destination the walker has not resolved yet.

**Why this exists.** `POST /goto` returns `200` and sets the target, but the walker only re-labels `nav_state` on its next ~150 ms tick. Without identity, this canonical loop lied:

```
POST /v1/move/goto {...}   -> 200 {"goal_id": 8, ...}
GET  /v1/observe/debug     -> nav_state: "arrived"   <-- but nav_goal_id: 7, the PREVIOUS goto!
```

Now the accept **atomically** bumps `nav_goal_id` and resets `nav_state` to `pending`, so the read above returns `nav_state: "pending", nav_goal_id: 8` — honest. **Rule: only trust a terminal `nav_state` (`arrived`/`no_path`/`search_exhausted`/`blocked`) when its `nav_goal_id` matches the `goal_id` the POST you are waiting on returned.** A lower id means you are still seeing an older goal's outcome; a matching id with `pending`/`planning`/`navigating` means your goal is genuinely in flight.

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
| `zone_line_not_in_map` | The requested `zone_id` **is** advertised by the server as a real exit, but the locally loaded zone geometry has no matching WLD zone-line (DRNTP) trigger region for it — a client-side `.wtr` map-data gap, not proof the exit doesn't exist in the real game. It is also omitted from `/v1/observe/zone_exits` (which only lists regions actually found in the loaded map), so "absent from `zone_exits`" does not by itself distinguish this from `no_zone_line_to_zone` — only `nav_reason` does. |

`nav_reason` for `blocked`:

| Reason | Meaning |
|--------|---------|
| `walker_stalled` | The fine planner *can* thread the route from here, and the walker still didn't move: a genuine collision/steering wedge. `POST /v1/move/manual` (optionally `"jump": true`) may free it; then re-issue the `goto`. |
| `local_no_way_through` | The **fine 2u planner closed its whole 40u window** without finding a way along the committed route, OR the walker spent its proactive-re-plan budget re-routing the same impasse without progress (the qcat L-corner class). The corridor is not threadable at the character's own collision radius — this is *not* a slide/collision wedge, and nudging will not fix it. A coarse route to the goal may exist, but the walker cannot follow it around this corner. Approach the goal from another direction. (#382, #378) |
| `fall_would_be_lethal` | The next waypoint is down a drop whose fall damage would likely kill the character. Stopped at the ledge. |

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

## The fine steering tier (`nav_local`) — #382

Navigation has two tiers. The **coarse** one (8 u cells, whole zone) chooses the route and produces
`nav_state`. The **fine** one (2 u cells, a 40 u window, re-planned every nav tick) is what actually
**steers** the character along the last few strides of that route — threading the thin ramps and narrow
openings the 8 u grid cannot see. `GET /v1/observe/debug` carries **`nav_local`**: what that tier last
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
| `planner_dead` | The fine worker thread has **died**. Steering has degraded to the coarse 8 u route for the rest of the session — the character **keeps walking**, but handles thin ramps and narrow openings worse. A client fault; restart to recover it. |

> **`nav_local.state` is never `no_path`, and structurally cannot be.** A 40 u window can never prove a
> *goal* unreachable, so a tight doorway must never be able to tell an agent its destination does not
> exist. Only the coarse planner, which closes the whole zone's frontier, may say `no_path`.

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

---

## Connection health

`GET /v1/observe/debug` carries ten fields that tell you **whether the rest of the payload can be
trusted at all**. Six are about what the SERVER did (below); four are about what the client itself
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
| `send_failures` | **Datagrams this client BUILT but could not put on the wire** — `try_send` returned an error. Cumulative since process start. `0` on a healthy client. |
| `send_failures_unretried` | The subset of `send_failures` with no client-side retransmit of that datagram. |
| `last_send_error` | `ErrorKind` name of the most recent send failure (`"WouldBlock"`, `"Uncategorized"`, …), or `null`. |
| `last_send_error_age_ms` | ms since that failure, measured at read time, or `null`. Distinguishes an old blip from an ongoing failure. |

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

- **`send_failures: 0` is the healthy reading.** Nonzero means the client hit a send error that many
  times since it started.
- **`send_failures_unretried` is the sharper number.** The complement (`send_failures -
  send_failures_unretried`) is the *reliable* stream: a failed reliable datagram is kept verbatim in
  the resend window and retransmitted until the server ACKs it, so it is delayed, not lost. The
  `unretried` ones — unreliable position updates, ACKs, keepalives, session control — are not
  re-sent by this client.
- **`send_failures_unretried > 0` does not by itself mean a command was lost.** Agent commands
  travel on the reliable path. What a climbing `unretried` count *does* mean is that the position
  firehose is dropping updates, i.e. the server's idea of where you are may have stopped tracking
  yours.
- **Use `last_send_error_age_ms` to tell "one blip at login" from "failing right now."** A count
  alone cannot distinguish them.
- Reported as `null` / `0`, never omitted, so absence of trouble is stated rather than inferred.

**If `snapshot_age_ms` is large, distrust the whole payload.** It means the client's own network
thread has stopped publishing, so every other field is a stale snapshot regardless of what
`connected` says.

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
- **`player`**, **`goal`** — position `[east,north,up]` at publish (**`null` when the position was
  not known** — fresh login before the first server placement, a zone reset; never a fabricated
  `[0,0,0]`); the active `/goto` goal.
- **`committed_coarse` / `committed_fine`** — the walker's **actual committed** coarse route and
  fine/local steering plan, verbatim (`Walker::path`/`local_path` — the #246 property; never a
  recompute).
- **`plan`** — the last coarse plan's record, from the planner's own reply: `gen`, `start`,
  `goal` (the question actually asked), `outcome` (`route`/`unreachable`/`exhausted`), `reason`
  (the `nav_reason` vocabulary), `route_len`, `plan_ms`, `tight`, `goal_snapped`, and **`trace`**:
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

`GET /v1/observe/debug` also carries **`nav_support`** — whether pathing in the current zone is
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
