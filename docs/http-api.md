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
| `GET /v1/observe/debug` | Player (zone, race, class, level, pos `[east,north,up]`, heading ccw/cw, `currency`, server_corrections, vitals `hp_pct`/`hp`/`hp_max`/`mana_pct`/`xp_pct`, target `target_id`/`target_name`/`target_hp_pct`) + **navigation** (`nav_state`, `nav_reason` — see [Navigation state](#navigation-state)) + **connection health** (`connected`, `link_age_ms`, `last_packet_age_ms`, `snapshot_age_ms`, `world_responsive`, `last_world_response_ms` — see [Connection health](#connection-health)) + camera state. |
| `GET /v1/observe/frame` | Current rendered frame as a PNG (`Content-Type: image/png`). |
| `GET /v1/observe/entities` | `{ "<name>": [x,y,z], ... }` for all known entities. |
| `GET /v1/observe/inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency}`. Slots are Titanium **wire** ids (DB general slots 23-30 → wire 22-29). |
| `GET /v1/observe/messages[?kind=npc]` | Machine-readable message log (oldest→newest). Each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `GET /v1/observe/spells` | The 9 memorized gems `{gem, spell_id, name}` (empty = null). |
| `GET /v1/observe/doors` | Current zone's doors `{door_id,name,x,y,z,heading,opentype,is_open}`. |
| `GET /v1/observe/zone_points` | Zone exit points received from the server. |

---

## `move` — movement

| Route | Body | Description |
|-------|------|-------------|
| `POST /v1/move/goto` | `{"name":"Guard Phaeton"}` \| `{"x":,"y":,"z":}` \| `{"map_x":,"map_y":}` \| `{}` | Walk to an entity (fuzzy name, one-time snapshot) or coordinates and **stop** on arrival. Empty body → the player's current target. `map_*` are Brewall map coords (= negated server x/y). |
| `POST /v1/move/follow` | `{"name":"a rat"}` \| `{}` | Walk to a named entity and **keep following** it until canceled. Empty body → current target. Coordinates are rejected (400). |
| `POST /v1/move/stop` | — | Cancel any active goto/follow. |
| `POST /v1/move/zone_cross` | `{"zone_id":N}` \| `{}` | Cross a zone line and send OP_ZoneChange (specific zone, or nearest line). |

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
| `idle` | Nothing to do. | — |
| `planning` | A route is being computed on the pathfinding worker thread. The character stands still. Normally < 1 s. | — |
| `navigating` | Walking a **complete route to your goal**. | `goal_z_snapped` (see below) or — |
| `navigating_partial` | Walking a **partial** route: the search was cut short, so this is *not* a route to your goal — it's progress toward a frontier, and it will re-plan from the far end. Usually resolves to `navigating` or `arrived`. | `search_node_cap` |
| `following` | A `/follow` chase has caught up; holding near the leader, still latched. | — |
| `arrived` | Reached the goal. | `goal_z_snapped` (see below) or — |
| `no_path` | **DEFINITIVE: no route exists.** The planner searched to completion. Do not retry the same goal — pick another. | see below |
| `search_exhausted` | The planner **gave up**. This is **"I don't know", not "no"** — a route may well exist. Try a nearer waypoint. | `search_node_cap` |
| `blocked` | A route exists, but the walker **could not follow it** (wedged after 8 recovery attempts). Not a routing failure. | `walker_stalled`, `local_no_way_through`, `fall_would_be_lethal` |

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

`nav_reason` for `blocked`:

| Reason | Meaning |
|--------|---------|
| `walker_stalled` | The fine planner *can* thread the route from here, and the walker still didn't move: a genuine collision/steering wedge. `POST /v1/move/manual` (optionally `"jump": true`) may free it; then re-issue the `goto`. |
| `local_no_way_through` | The **fine 2u planner closed its whole 40u window** without finding a way along the committed route. The corridor is not threadable at the character's own collision radius — this is *not* a slide/collision wedge, and nudging will not fix it. Approach the goal from another direction. (#382) |
| `fall_would_be_lethal` | The next waypoint is down a drop whose fall damage would likely kill the character. Stopped at the ledge. |

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

`GET /v1/observe/debug` carries six fields that tell you **whether the rest of the payload can be
trusted at all**. They are computed when you ask — not cached — so nothing has to be running inside
the client for them to be right (#343).

| Field | Meaning |
|-------|---------|
| `connected` | **Is the link up?** `false` after 15s with no inbound datagram of any kind. Use this for "am I disconnected?" — but it only proves the SOCKET ACKs, not that the world is alive (see `world_responsive`). |
| `link_age_ms` | ms since any inbound UDP datagram, session-layer ACKs included. `connected` is derived from this. |
| `last_packet_age_ms` | ms since the last *world update* (an application packet). |
| `snapshot_age_ms` | ms since the client's network thread last ticked. |
| `world_responsive` | **Is the WORLD alive, not just the socket?** `false` only when an active liveness probe went unanswered past its bound while the link kept ACKing — a wedged zone. `true` for a healthy zone, including a legitimately idle one (the probe is answered). `true` before the first probe fires. See below. |
| `last_world_response_ms` | ms since the world last *proved* it processed something for us — a probe reply or a spontaneous packet, whichever is fresher. The companion to `world_responsive`. |

**`last_packet_age_ms` is not a disconnect signal.** An idle EQ session — a character sitting alone
in a quiet zone — routinely goes **40+ seconds with no application packet** while the link is
perfectly healthy (the server only pushes HP/mana/position on *change*). Treating a high
`last_packet_age_ms` as a dead connection will send an agent into a pointless reconnect loop. Read it
as *"the world is quiet"*, and use `connected` to decide whether the link is gone.

> **Changed in #343.** `connected` previously derived from application traffic and was recomputed
> only when a frame rendered — so a dead connection (no packets → no render) reported
> `connected: true`, frozen, forever. It now derives from link liveness, at read time.

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
