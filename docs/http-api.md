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
| `GET /v1/observe/debug` | Player (zone, race, class, level, pos `[east,north,up]`, heading ccw/cw, `currency`, server_corrections, vitals `hp_pct`/`hp`/`hp_max`/`mana_pct`/`xp_pct` plus [`hp_verified`](#hp_verified--is-the-hp-in-this-payload-the-servers-1005) — **read it before acting on `hp`**: `false` means at least one of those three vitals is a number the client inferred, not a figure the server sent, `levitating` (three-valued `true`/`false`/`null` — see [`levitating`](#levitating--three-valued-levitate-buff-state-not-a-gravity-reading-598)), target `target_id`/`target_name`/`target_hp_pct`/`target_con`/`target_attitude`/`target_level`) + **navigation — SPLIT ACROSS TWO NESTING LEVELS; this grouping is by topic, not by where the field lives** (under `player`: `nav_state`, `nav_reason`, `position_provisional`, `crossing_pending_ms`. Top-level, siblings of `player`, NOT under it — same convention as `last_consider`: `nav_goal_id`, `nav_goal`, `nav_blocked_by`, `nav_tier`, `nav_declined_pads`, `nav_local`, `nav_local_planner_dead`, `nav_stall`, `nav_support`, `nav_tight`; they sit outside `player` because that object is already at serde_json's macro recursion limit — see [Navigation state](#navigation-state), [The fine steering tier](#the-fine-steering-tier-nav_local--382) for `nav_local` and [`nav_local_planner_dead`](#nav_local_planner_dead--fine-planner-liveness-session-scoped) — the **session-scoped** fine-planner liveness flag, the one nav field that is always present rather than `null` when healthy, and the one to poll for a dead fine planner because `nav_local` retires with the goal — and [`nav_declined_pads`](#nav_declined_pads--the-teleport-pads-nav-refused-offered-back-to-you-543--266)) + **connection health** (`connected`, `link_age_ms`, `last_packet_age_ms`, `snapshot_age_ms`, `world_responsive`, `last_world_response_ms`, `send_failures`, `send_wouldblock_rescued`, `send_deferred`, `send_starved`, `send_failures_unretried`, `last_send_error`, `last_send_error_age_ms`, `reliable_abandoned` — see [Connection health](#connection-health)) + **`net_thread_dead`** (`null` while the network thread is alive; a reason string once it has died and the whole payload is a frozen final snapshot — see [net_thread_dead](#net_thread_dead--the-frozen-worlds-terminality-634)) + **`zone_map_load`** (`null` while this zone's map-labeled fallback entries in `zone_entrances` loaded fine (or none were needed yet); `{reason, detail}` once that `.txt` read failed — see [`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816)) + **`server_pushed_rosters`** (top-level, ALWAYS present: the `doors`/`entities`/`zone_entrances` rosters with the count `held` and `complete: null`, plus the `no_completeness_signal` sentence saying why `complete` is never anything else — see [`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073)) + **`zone_cross_best_effort`** and **`zone_cross_stopped`** (top-level, `null` while there is nothing to disclose — see [Zone-cross degradations you can detect](#zone-cross-degradations-you-can-detect-713)) + **`last_consider`** (spawn-scoped result of the most recent consider of ANY spawn, target or not — see [Consider results](#consider-results)) + **camera state** (`camera`, describing the last frame ACTUALLY DRAWN, not the current tick — read `drawn_frame`/`drawn_age_ms` first, and note that the `snapshot_age_ms` in the same payload is the network clock and does not age it; see [Camera freshness](#camera-freshness-drawn_frame--drawn_age_ms-867)). |
| `GET /v1/observe/frame` | Current rendered frame as a PNG (`Content-Type: image/png`). **503 while the zone's assets are still loading** — see [`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579); `?allow_pending=1` opts past it. Optional `preset`/`pitch`/`yaw`/`distance` params request a one-off diagnostic camera angle for just this capture — see [Camera override for `/frame`](#camera-override-for-observeframe-422). |
| `GET /v1/observe/entities[?labeled=1]` | Default: `{ "<name>": [x,y,z], ... }` for all known entities, with same-base-name + byte-identical-position duplicates collapsed (#471 — suspected server-side `spawn2` duplication; the model is untouched so each instance is still targetable by its full name). `?labeled=1` returns the richer `{count, entities:{"<name>":[x,y,z]}, deduped, duplicate_groups:[{position,names,kept}], note, poses, snapshot_age_ms}` exposing which duplicates were collapsed, plus **`poses`** (#643): `{"<name>": {pose, gait}}`, keyed **exactly** like `entities` — the two are projected under one lock, so indexing `poses` by any name in `entities` is safe. `pose` is the server-published body state — `standing`/`freeze`/`looting`/`sitting`/`crouching`/`lying`, or **`unknown(<raw>)`** when the server sent a code this client does not recognise (reported verbatim, never guessed at). `gait` is the signed locomotion-speed code from the entity's last position update (~12 at walk, 28 at full run, negative when backing up); **`null` means "no position update yet", NOT "standing still"**. The default bare-map shape carries the same freshness value in the `X-Snapshot-Age-Ms` header instead — see [Per-endpoint freshness](#per-endpoint-freshness--snapshot_age_ms-646). **An empty body does not mean "this zone is empty"**: zone-in clears the published roster (#1010/#1063) and it refills from spawn packets, so during that window `{}` is "not published yet" — and there is no `ready` gate to wait for, because this endpoint is not derived from loaded geometry. See [`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073). |
| `GET /v1/observe/inventory` | `{count, items:[{slot,item_id,name,charges,icon,idfile}], currency, coin_verified, snapshot_age_ms}`. Slots are Titanium **wire** ids (DB general slots 23-30 → wire 22-29). |
| `GET /v1/observe/messages[?kind=npc]` | Machine-readable message log (oldest→newest). `{count, messages, snapshot_age_ms}`; each line `{kind, text, keywords}`; `kind` ∈ npc/chat/combat/system/exp/loot/trade/zone. This is how you read NPC dialogue. |
| `GET /v1/observe/dialogue` | Pending NPC dialogue/quest choices `{count, choices:[{index, text}], snapshot_age_ms}`. |
| `GET /v1/observe/spells` | The 9 memorized gems `{gems:[{gem, spell_id, name}], snapshot_age_ms}` (empty = null). |
| `GET /v1/observe/skills` | All skills with current trained value `{skills:[{id, name, value}], snapshot_age_ms}`; `value == 0` means untrained. |
| `GET /v1/observe/doors` | Current zone's doors — a bare array `[{door_id,name,x,y,z,heading,opentype,is_open}]`; freshness rides the `X-Snapshot-Age-Ms` header (no room for a JSON key on a bare array). **`[]` does not mean "this zone has no doors"** — the roster is server-pushed and zone-in empties it, so an empty body is "no record held", not "none exist"; see [`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073). |
| `GET /v1/observe/zone_entrances` | Zone entrance points received from the server (arrival side — see [Navigation state](#navigation-state) for the distinction from `zone_exits`), plus a handful of client-synthesized entries read from the CURRENT zone's own map (the heuristic only ever recognizes a label naming North/South Qeynos or Qeynos2, but — measured — five zones' shipped map packs actually carry such a label: see [`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816) for the list and method). Also served at the deprecated alias `GET /v1/observe/zone_points`. A bare array; freshness rides the `X-Snapshot-Age-Ms` header. **If those synthesized entries failed to load, this list is silently short** — check [`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816) on `/v1/observe/debug`. This same list also backs `POST /v1/move/zone_cross`'s reachable-`zone_id` check and the walker's `no_zone_line_to_zone` result — a load gap here is not only a reporting gap, it can change what a crossing request does. Separately from that load gap, **`[]` does not mean "this zone has no entrances"**: zone-in empties this list too (#1010/#1063) and the server-advertised entries refill on no schedule the client controls — see [`server_pushed_rosters`](#server_pushed_rosters--what-an-empty-roster-means-939-1073). |
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
| `POST /v1/move/goto` | `{"name":"Guard Phaeton"}` \| `{"x":,"y":,"z":}` \| `{"map_x":,"map_y":}` \| `{}` | Walk to an entity (fuzzy name, one-time snapshot) or coordinates and **stop** on arrival. Empty body → the player's current target. `map_*` are Brewall map coords (= negated server x/y); `z` is optional there (defaults, then the walker snaps to the actual floor) but **required** on the raw `{x,y,z}` form. A body with SOME coordinate field(s) but not a complete `{x,y,z}` or `{map_x,map_y}` — e.g. `{"x":,"y":}` with no `z` — 400s naming exactly which field(s) are missing (`partial target: got {x, y} but missing {z}`), never the "no target; provide a name or coords" message, which is reserved for a body with no coordinate field at all (#886). **Two or more target forms in one body never silently pick a winner** — see [Target-form precedence and `ignored_fields`](#target-form-precedence-and-ignored_fields-901) (#901). **Returns JSON**, including [`matched`](#matched--which-entity-a-name-actually-resolved-to) when the goal came from a name/target. |
| `POST /v1/move/follow` | `{"name":"a rat"}` \| `{}` | Walk to a named entity and **keep following** it until canceled. Empty body → current target. Coordinates are rejected even alongside `name` (400 — but a freezing hold answers 409 first, #884) — no target-naming field is ever silently dropped here (#901). `avoid_aggro`/`aggro_buffer` are a separate field family: `/follow` shares `MoveBody` with `/goto`, and until #952 it accepted both knobs and never applied them. It now applies them, to the same shared nav setting `/goto` and `/zone_cross` write, and past every 4xx return — so a refused `/follow` changes nothing. (`/zone_cross` is the exception and predates #952: it applies the knobs *before* its own `zone_id … is not reachable` 400, so a refused crossing does still move the shared setting. Measured and pinned by `a_refused_zone_cross_does_write_the_avoid_knobs_pre_existing_divergence`; unchanged here.) **Returns JSON** with [`matched`](#matched--which-entity-a-name-actually-resolved-to). |
| `POST /v1/move/stop` | — | Cancel any active goto/follow. **Not** subject to the `held` gate below — it is a cancel, its effect is on the nav slots and not on the physics step, so it stays honest and available while the body is frozen. |
| `POST /v1/move/zone_cross` | `{"zone_id":N}` \| `{}` | Cross a zone line and send OP_ZoneChange (specific zone, or nearest line). |

**All five of `goto`/`follow`/`zone_cross`/`manual`/`jump` answer `409 Conflict` with JSON
`{"status":"held", "hold":{…}, "message":…}` while the character is physics-held in a way that
freezes the controller's step (`player.hold.reason == "embedded_no_recovery"`) — nothing is queued
and no `nav_goal_id` is stamped.** See [`hold`](#hold--the-character-is-physically-stuck-and-the-client-cannot-free-it-724).
`goto` and `follow` additionally carry a `hold` key on their **200**: `null` for a healthy body, and
the same object when a hold is in force that does *not* freeze the step (`underworld_no_recovery`) —
so an accepted goal never hides the fact that the body is hanging out of the world (#884).

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
| `POST /v1/interact/loot` | `{"id":N}` \| `{"name":"..."}` \| `{}` | Loot a corpse (specific id, fuzzy name, or nearest). `id` and `name` are **mutually exclusive** — both in one body is a `400` (#952/#956). |
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

## `camera`

| Route | Body | Description |
|-------|------|-------------|
| `GET /v1/camera` | — | The orbit camera **as of the last frame actually drawn** (azimuth, elevation, radius, focus, mode, plus the rendered `eye`/`occluded`/`still_blocked` and the freshness pair `drawn_frame`/`drawn_age_ms`). Not "now" — see [Camera freshness](#camera-freshness-drawn_frame--drawn_age_ms-867). |
| `POST /v1/camera` | `{"azimuth":,"elevation":,"radius":,"focus":[x,y,z]}` (all optional) | Set the orbit camera. Returns 200 when the command is **queued**, not when it has been applied or drawn — poll `drawn_frame` to see it land. |
| `POST /v1/camera/reset` | — | Reset to the default follow view. |

### Camera freshness: `drawn_frame` / `drawn_age_ms` (#867)

Every field of the camera block — on `GET /v1/camera` and in `/v1/observe/debug`'s `camera` object
alike — describes **one frame**: the one named by `drawn_frame`. The render loop publishes the whole
struct in a single write, and only after a frame has actually been encoded. On a tick that returns
early from the surface-acquire match (`Lost`/`Outdated`/`Timeout`) nothing is published and the
previous values stay in place, including the four *desired-framing* fields, which an earlier
`/v1/camera` Set in that same tick may already have changed.

| Field | Meaning |
|-------|---------|
| `drawn_frame` | Monotonic index of the frame the block describes. `null` = **no frame has been drawn yet** — the startup seed, served from process start through GPU init and the first zone load. |
| `drawn_age_ms` | Milliseconds since that frame was drawn, computed when the response is encoded. `null` whenever `drawn_frame` is. |

**The staleness is unbounded.** The event loop stops requesting redraws 300 ms after the last
activity, so a surface that keeps failing to acquire — a minimised or occluded window, not just a
resize — freezes the camera block indefinitely, with every other field looking exactly as it does on
a healthy tick. Read `drawn_frame`/`drawn_age_ms` before trusting the rest; `drawn_frame` unchanged
across two reads means nothing was drawn in between.

**`snapshot_age_ms` does not age this block.** That field is the *network* health clock
(see [Connection health](#connection-health)) and reads fresh throughout a rendering stall.

---

## `lifecycle`

| Route | Description |
|-------|-------------|
| `POST /v1/lifecycle/camp` | Toggle a camp (start, or cancel one in progress). A completed camp shuts the client down cleanly with no linkdead. |
| `POST /v1/lifecycle/exit` | Camp out (idempotent `Start`), then cleanly shut the process down (~30s). **Deliberately not gated on a live session** — tearing a zombie session down is what it is for. When `net_thread_dead` is non-null no camp can be sent (nothing is left to drain it), so the 200 body says so instead of promising a camp-out and relays the thread's own reason verbatim. Which body you get depends on WHICH state the thread is in, because the three are not interchangeable: a lost session (panic / fatal error / unexpected return) warns that the server declares a **linkdead** drop on its own timer — this shutdown inherits that, it does not cause it; a shutdown already under way says so and reports that this request changes nothing; and `--testzone`, where the thread was never started, says there is no server session at all and nothing is left linkdead. The first and third also say the process will exit via the 45s watchdog rather than in ~30s; the already-shutting-down one does not, because that process exits through its own main loop instead. The teardown proceeds in every case (#890). **Residual:** a net thread that is merely *wedged* has not ended, so `net_thread_dead` is still `null` and this endpoint still answers with the camp-out body even though nothing will drain that camp — cross-check `snapshot_age_ms`. |
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
| `idle` | Nothing to do. **Every `idle` the client publishes after start-up carries a `nav_reason` saying how it got there**, so `nav_state: "idle"` with `nav_reason: null` means exactly one thing: no nav request has been made since this client started (#725). It is otherwise a real outcome, not an absence of one. | `zoned`, `stopped`, `goto_superseded`, `goal_dropped`, `respawned`, `hp_restored`, `zone_cross_dropped_unhandled` — all below; `null` **only** at start-up |
| `planning` | A route is being computed on the pathfinding worker thread. The character stands still. Normally < 1 s. | — |
| `navigating` | Walking a **complete route to your goal**. | `goal_z_snapped` (see below) or — |
| `navigating_partial` | Walking a **partial** route: the search was cut short, so this is *not* a route to your goal — it's progress toward a frontier, and it will re-plan from the far end. Usually resolves to `navigating` or `arrived`. | `search_node_cap` |
| `navigating_stalled` | **A route is committed and the walker is NOT executing it.** The body has neither advanced its route cursor nor improved its closest approach to the goal for `NAV_STUCK_TICKS` (20) walker ticks — about 3 s. **Only fixed-destination goals reach this state** — see the limitation under `nav_stall` below. This is **not terminal** — it is not on the terminal list below, and the walker goes on backing off and re-pathing under it. **The verdict latches:** a re-path or a back-off does not clear it. It exists because the alternative is worse — before #851 a walker circling under a ledge published plain `navigating` for the whole ~32 s it spent recovering, and an agent polling `nav_state` had no way to tell it apart from a walk that was working. Read **`nav_stall`** (below) for how long and how many re-paths. If the walker never recovers you will eventually get `blocked` (`walker_stalled` or `local_no_way_through`, at 8 re-path attempts) or `blocked`/`no_progress` (60 s). | `goal_z_snapped` (see below), `search_node_cap`, or — (it carries whatever reason the committed route carries) |
| `following` | A `/follow` chase has caught up; holding near the leader, still latched. | — |
| `arrived` | Reached the goal. | `goal_z_snapped` (see below) or — |
| `no_path` | **No route was published for this goal — read `nav_reason` before concluding one cannot exist.** For most reasons it is definitive: the planner searched to completion, so do not retry the same goal, pick another. **Not all of them are.** `planner_dead` means the pathfinding worker died, and on `/move/zone_cross` the `region_data_*` reasons (#815) mean the zone's region map could not be read — neither is a completed search, and both are **"I don't know", not "no"** — the same reading `search_exhausted` carries, but reported under this state rather than that one. The state itself is still terminal — nothing will retire it for you — so the retry decision is `nav_reason`'s to make, not this row's. | see below |
| `search_exhausted` | The planner **gave up**. This is **"I don't know", not "no"** — a route may well exist. Try a nearer waypoint. | `search_node_cap` |
| `blocked` | A route exists, but the walker **could not follow it** (wedged after 8 recovery attempts). Not a routing failure. | `walker_stalled`, `local_no_way_through`, `fall_would_be_lethal` |
| `zone_loading` | **This client has no *usable* model of the zone the character is in yet** — its terrain/collision are still loading, their load failed, or the loaded grid still belongs to the zone the character just LEFT (the stale window, #600). No search was run and no route exists to report; the goal is kept and planned for real once the correct zone's assets land. Since #600 the walker refuses through the SAME `zone_assets::usability` predicate the HTTP world endpoints use, so the reason is that predicate's own verdict — read `zone_assets` (below) for the matching detail. | `zone_assets_pending`, `zone_assets_failed`, `zone_assets_idle`, `zone_assets_stale_for_previous_zone`, `player_zone_unknown` |
| `dead` | **The character is slain** — the server sent `OP_Death`, navigation was abandoned because a corpse cannot move (#238, #644), and **`player.dead` in the same payload reads `true`**. That agreement is load-bearing and is what #1000 fixed: this word used to be published for the HP disjunct as well, so `nav_state: "dead"` sat beside `dead: false` in one payload and the reader had no way to tell which half was lying. The HP-only case is now [`halted_hp_zero`](#nav-state-halted-hp-zero) (next row) and this word means confirmed death and nothing else. It is honest in the way `idle` is not: an agent that issued a goto and then polled must be able to tell "you died and went nowhere" from the ambiguous `idle` (which also means "ready for work"). **It is not on the terminal list** and does not need to be — it is re-published by every walker tick that still finds the character dead, so it cannot be retired out from under you the way #725 describes; it clears to `idle` with `nav_reason: "respawned"` once the character is up again. **A movement command issued *while* dead is not accepted at all** — `POST /v1/move/{goto,follow,zone_cross,manual,jump}` returns **`409 Conflict`** with a machine token `dead` (JSON `"status":"dead"` on `/goto` and `/follow`; the text body names `dead` on the others), so you never get a `200 … navigating` for a goal a corpse can never reach. Respawn (`POST /v1/lifecycle/respawn`) before reissuing. **Since #1000 that token is the same word as `nav_state`**, so the HP-only halt refuses under `"status":"halted_hp_zero"` instead — a client matching only on `dead` will now fall through to its unknown-refusal branch in that case, which is correct, because the condition is not a death. Match both words if you want every life halt. **A physics-held character is refused the same way but under a different token** — `409` with `"status":"held"` when `player.hold.reason` is `embedded_no_recovery` (#884); see [`hold`](#hold--the-character-is-physically-stuck-and-the-client-cannot-free-it-724). The two are independent gates and `dead` is checked first, so a dead *and* held character reports `dead`. | `player_dead` |
| <a id="nav-state-halted-hp-zero"></a>`halted_hp_zero` | **Navigation is halted on an HP reading, and the character is NOT confirmed dead** (#1000). The client's published `hp` is at or below 0 with a known `hp_max`, and **no `OP_Death` has arrived** — so `player.dead` in the same payload reads `false`, and that is agreement, not contradiction: this word exists precisely to say "the halt is the HP disjunct, not a death". The halt itself is the same halt `dead` describes and fires on the same predicate as before; only the *word* is split, so nothing about when navigation stops has changed. **Do not respawn.** `POST /v1/lifecycle/respawn` addresses nothing when nothing died, and the `409` you get from `/v1/move/*` under this state says so and prescribes no respawn — it quotes the `hp`/`hp_max` reading it rests on, and its machine token is `halted_hp_zero` (JSON `"status"` on `/goto` and `/follow`), not `dead`. The refusal set is unchanged from before the split: the same movement routes are refused under the same `409`, for the same predicate. Read `hp`/`hp_max`: the halt clears to `idle` with `nav_reason: "hp_restored"` on its own as soon as an authoritative update puts `hp` back above 0. If it does not clear while the character is plainly alive, the HP reading is stale or otherwise not the server's — report it (#1005 is one route by which that happened, and is addressed separately). Like `dead`, it is not on the terminal list and is re-published every tick the predicate still holds. | `hp_zero_unconfirmed` |

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

**The two life-halt words sit outside this scheme on purpose (#1007).** `dead` and `halted_hp_zero`
are not on the terminal list, yet they do not retire the way the rule above describes and they must
not: they are not a goal's outcome, they are a statement about the character. They are re-published
by every walker tick whose predicate still holds, and they are cleared by that same tick — to `idle`
with `respawned` or `hp_restored` — the moment it does not. Two consequences worth knowing as a
reader: a goal-level retirement (`/stop`, a supersede, a dropped goal) does **not** relabel them, and
they cannot get stuck, because clearing them is not conditional on any goal existing. Adding either
word to `TERMINAL_NAV_STATES` would break exactly that — the clearing path is guarded on
non-membership, so the halt would become permanent. The code says so at the array.

### `nav_stall` — a committed route the walker is not executing (#851)

`GET /v1/observe/debug` carries **`nav_stall`** (top-level, a sibling of `player`, not under it).
It is `null` except while `nav_state` is `navigating_stalled`, and the two are written together from
one verdict under one lock hold — you will never see one without the other. **Both belong to the goal
in `nav_goal_id` and to no other:** the walker's stall verdict carries the goal id it was measured
against, and reading it for a different goal is not something the client can express — so neither
this payload nor the `navigating_stalled` word can outlive the goal that earned them.

```json
"nav_stall": {
  "quiet_ticks": 34,
  "quiet_ms":    5310,
  "repaths":     2,
  "route":       "complete",
  "detail":      "…"
}
```

| field | meaning |
|-------|---------|
| `quiet_ticks` | Consecutive walker ticks (150 ms each) with **no** route-cursor advance and **no** closest-approach improvement. Crosses the threshold at 20, so the smallest value you can ever read here is 20. |
| `quiet_ms` | The **same window as `quiet_ticks`**, in wall-clock milliseconds: measured from the walker's last progressing tick. So the first `navigating_stalled` you see already reads ≈ 3000, not `0`. It is measured, never computed as `quiet_ticks × 150`, and that is the only reason the two can disagree — the 150 ms nav tick is a floor, not a guarantee, so under load this runs **longer** than the arithmetic (the example above is 34 ticks in 5310 ms). Use `quiet_ticks` as the evidence count and this as the clock. |
| `repaths` | How many recovery re-paths the walker has spent on this goal. It gives up at 8, which is where the terminal `blocked` comes from — reason `local_no_way_through` if the fine planner also says there is no way through at that moment, `walker_stalled` otherwise. |
| `route` | `complete` (the committed route ends at your goal) or `partial` (it ends short of it). A stall on a `partial` means the walker is not executing even the partial. |
| `detail` | A prose restatement of the four fields above, for an agent reading the JSON without this document. Present whenever `nav_stall` is. |

**What it does and does not mean.** It means: *a route is committed and the body is not moving along
it.* It does **not** mean the goal is unreachable — the walker is still in back-off/re-path recovery
and may escape it. Treat it as "give this a few more seconds, and if it persists, expect a `blocked`
verdict rather than an arrival".

**Limitation: this state is only ever published for a fixed destination** — `/move/goto` and
`/move/zone_cross`. A `/move/follow` chase is driven down a different path that does not run the
stall verdict at all, so a chase reads `navigating` (or `navigating_partial`, or `following` once it
has caught up) and **never** `navigating_stalled`, even while it is genuinely wedged. Measured, not
argued: `a_wedged_follow_chase_is_not_reported_as_stalled_at_all_851` drives the real walker tick
with a body that does not move for 60 ticks (~9 s) under a chase and reads `navigating` for every one
of them; the same fixture without the chase reaches `navigating_stalled` at tick 20. So for a
`/follow`, `nav_state` alone does not distinguish a chase that is walking from one that is stuck —
poll the character's position. Closing this gap means deciding what "progress" means for a moving
target, which is a separate change (#929).

**Why it exists.** The stall was always *detected* — the back-off/re-path machinery has fired at 20
quiet ticks for a long time — it was never *published*. So a walker circling under a ledge for ~32 s
and a walker making perfect progress produced the identical `nav_state: "navigating"`, and an agent
polling that field had nothing to distinguish them. #851.

### Every `idle` says how it got there (#725)

`idle` used to be published bare from several different places, and one of them was the **success**
path of `/v1/move/zone_cross` — so a successful crossing and a request the client had thrown away
looked byte-identical to a polling agent (`"nav_state":"idle","nav_reason":null` in both). Each of
those call sites now names itself. The complete set of ways to reach `idle`:

| `nav_reason` | Meaning |
|--------------|---------|
| `zoned` | **The character changed zone**, and navigation was reset because a route computed in the old zone means nothing in the new one. This is the `nav_state` a *successful* `/v1/move/zone_cross` ends at — read it together with `player.zone`, which is the authoritative statement of where you are. It is deliberately about the zone change and not about the request, so it is equally true of a GM `#zone`, a gate/evac, or a portal door. Not an error. |
| `stopped` | **You asked** — `POST /v1/move/stop` was accepted and any goto/follow/queued zone-cross was cancelled. **One exception, and it is deliberate (#1007):** if `nav_state` is a life halt (`dead` or `halted_hp_zero`) when the `/stop` lands, the goal really is cancelled but the published word stays the halt word — because a `/stop` says something about your goal and nothing at all about whether you are alive, and relabelling the halt `idle` would tell you the halt cleared. The fresh `nav_goal_id` is your confirmation the `/stop` landed. **Do not wait for a `409` here:** `/stop` is a cancel and is deliberately outside the life-halt gate, so it never answers `409` under a halt — a live session always answers `200`, and its only other answer is a `503` from the session-liveness guard (`require_live_session` — net thread not running, not connected, or not ticking). The same preservation covers `goto_superseded`, the other `idle` retirement stamped through the same writer. It does **not** cover `zone_cross_dropped_unhandled`: `ZoneCrossTicket::drop` publishes that one straight through `NavStatus::retire_to_idle`, bypassing the guard, so a zone-cross ticket dropped unhandled while halted can publish `idle` for a single tick before the walker republishes the halt word on the next one. |
| `goto_superseded` | You did **not** ask: something else took over steering — manual movement (keyboard or `POST /v1/move/manual`), or the auto-melee-engage override. Your goto is gone; reissue it if you still want it. |
| `goal_dropped` | Your goal stopped existing without being reached — e.g. a `/follow` target despawned, or a request was cancelled from elsewhere in the client. Not an error about the route; there is simply nothing left to walk to. Reissue if you still want it. |
| `respawned` | The `dead` state cleared because the character came back up (#644) — a real death ended. Since #1000 it is published **only** for `dead`; the HP-only halt retires under `hp_restored` instead, so this word never claims a respawn that did not happen. |
| `hp_restored` | The `halted_hp_zero` state cleared because `hp` came back above 0 (#1000). **Nothing died and nothing respawned** — that is the whole reason it is not `respawned`. |
| `zone_cross_dropped_unhandled` | **A client bug, reported instead of hidden.** Your `/move/zone_cross` was consumed by the client and produced no outcome at all — no walk, no crossing, no refusal. Nothing is in flight and nothing will happen; retry, or use `/move/goto`. If you see this, please file it with the zone and your position: it means a code path took your request and wrote nothing, which is exactly the defect the backstop that emits this reason exists to make visible (#725). |

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

### `hold` — the character is physically stuck and the client cannot free it (#724)

`player.hold` is `null` for a healthy character — **including one that is simply standing still** —
and non-null only while the movement controller has stopped the body and has no way to resume.

It exists because those two states were indistinguishable through this API. `pos` is correct in
both. `nav_state` is `idle` in both. `nav_state.stuck_ticks` is the *walker's* counter and only
advances while a `/goto` is actively driving, so a character that was summoned into a rock and is
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
| `embedded_no_recovery` | The body **cannot be placed**: geometry pierces its footprint **or** there is no floor within 200 u below its feet. The push-out search found nowhere it can legally stand, there is no recovery position to fall back to (a position discontinuity — a GM summon, a large server correction — supersedes that history, #724), and the zone-wide last-resort search found nowhere either. | **No.** Physics is frozen: the controller's step returns before it reads driver input, so no wish of any shape moves the body. Since #884 the movement endpoints **refuse** (`409`, `"status":"held"`) rather than accept — before #884 they answered `200` and produced no motion. |
| `underworld_no_recovery` | The body fell to the zone's **underworld floor** and the client is holding it there rather than let it drop out of the world (#150), with no recovery position to restore. It is hanging: not falling, not landing, not grounded. | Horizontally, yes — but there is probably nothing under it. |

⚠️ **`embedded_no_recovery` does not mean geometry is inside the body.** It is the client's
`is_embedded` predicate, which is a disjunction: pierced footprint *or* an empty column. #845's live
casualty was measured (against the zone's own baked collision) to be the second — zero triangles
over the column, nearest ground 133 u away — while this table and the `detail` string both said
"embedded in world geometry". Nothing in the API distinguishes the two halves; if you need to know
which, look at the geometry, not at this field.

**Since #845 the client no longer only reports this state — it also tries to leave it.** When both
the push-out and the recovery ring come up empty, the client searches the zone (out to 512 u,
retried about once a second) for anywhere a body could legally stand, and relocates itself there.

⚠️ **This does not mean the hold you are looking at is about to clear. It means the opposite.**
Both directions were measured and both run the other way round:

- **A succeeding search never publishes this field at all.** The relocation happens inside the
  physics step and returns *before* the hold is raised, so `player.hold` stays `null` throughout —
  measured at **0 held frames out of 300** in a zone the search can solve. There is no
  `Some(..) → null` transition to watch for, because there was never a `Some(..)`.
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

⚠️ **There is currently no reliable way to detect a client-side relocation through this API** —
including the one #845 introduced, which can move the body up to 512 u without the driver asking.
There is no dedicated field, and the two that look like they might serve do not:

- **`player.hold` does not**, for the reason above: a succeeding relocation never publishes one.
- **`player.server_corrections` does not advance for one** — correctly, since it is not a server
  correction. But that is all it tells you. It has exactly one incrementer, in the network layer's
  `OP_ClientUpdate` handler, gated on the server-vs-client horizontal delta exceeding 12 u; it is a
  counter of large *server-driven* snaps, and it is not a function of `player.pos` changing at all.
  A body standing perfectly still leaves it unchanged, and so does an ordinary `/v1/move/goto` walk
  (that handler treats sub-12 u deltas as normal sync lag and does not count them). "Unchanged"
  therefore carries no information about whether anything moved you.

What you will actually see is a bare `pos` jump with nothing to attribute it to. The relocation *is*
recorded, at `warn`, in the client log — `controller RELOCATED [#845]: … moved N u to …`, with the
origin and destination — but that channel is not reachable from this API. Tracked as **#925**; do
not build an agent behaviour on inferring it until there is a field for it.

**A persistent hold still needs an outside push, and an ordinary character has no client-API way to
produce one.** Every movement endpoint (`/v1/move/manual`, `/v1/move/jump`, `/v1/move/goto`,
`/v1/move/follow`, `/v1/move/zone_cross`) depends on the physics step an `embedded_no_recovery` hold
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

and **nothing is queued**: no goto/follow target is latched, no zone-cross request is enqueued, no
manual-move wish is set, and `nav_goal_id` is not advanced — the refusal is decided before any of
that, the same shape as the `dead` gate above. The embedded `hold` object is the same one
`/v1/observe/debug` serves under `player.hold`, so a caller that only ever sees the `409` still gets
the full reason, `held_secs` and `detail` without a second request.

`POST /v1/move/stop` is deliberately **not** gated: it is a cancel, its effect is on the nav slots
rather than on the physics step, and "the goal is cleared" is true of a frozen body.

`underworld_no_recovery` is deliberately **not** refused, for the reason above — lateral wishes still
reach the body there, and `/v1/move/manual` walking out is that state's only client-API exit, so
refusing it would be the same lie in the opposite direction. Instead, `/v1/move/goto` and
`/v1/move/follow` carry a `hold` key on their **200** body: `null` when there is no hold, and the
same hold object when one is in force that does not freeze the step. An accepted goal therefore never
hides the fact that the body is hanging under the world.

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
swimmer making perfect progress: `pos` barely moved, `nav_state` read whatever the driver had last
set, and the one nearby stall counter — `nav_local.stuck_ticks`, a top-level sibling of `player`,
not a key inside it — advances only while a `/goto` is driving, so a manually-driven or
directly-wished swimmer never touched it. Every served field said "swimming normally", which is the
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
makes it true is one `player.insert("afloat_stall", …)` in `crates/eqoxide-http/src/observe.rs`'s
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
| `GET /v1/observe/frame` | A PNG of the placeholder ground plane is indistinguishable from a genuinely empty zone, and a `stale` frame shows the zone you left. Pass **`?allow_pending=1`** if the loading screen is what you actually want. |

`zone_exits` has a **second, unrelated 503** that this readiness verdict does not cover — see
[Zone exits: `[]` means exactly one thing (#803)](#zone-exits--means-exactly-one-thing-803).

Every `200` from `/v1/observe/frame` also carries **`X-Zone-Assets-State:`** with the same word as
`zone_assets.state`, so a PNG fetched with `?allow_pending=1` cannot be mistaken downstream for one
of the real zone. Only `ready` means the image shows the zone the character is in. It also carries
**`X-Snapshot-Age-Ms`** (#646 — see [Per-endpoint freshness](#per-endpoint-freshness--snapshot_age_ms-646)):
a PNG body has no room for an in-band field, so the same freshness clock every other endpoint
carries rides this header instead.

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
landed, packet capture (`GET /v1/observe/packets`, opcode `0x7291`) records `OP_SpawnDoor`
arrivals — but **capture is
default-off and not retroactive**, so it must already have been enabled when the record landed.
Enabling it *after* you see `[]` returns `count: 0` with `enabled: true`, which is the same
false-negative this paragraph is warning you about, wearing a different hat. Check the `enabled`
field before reading any conclusion into a zero count. Treat a persistent `[]` as *not yet known*,
not as *this zone has no doors*, until the zone-in has otherwise finished.

`POST /v1/move/goto` still accepts the goal, but its response carries a non-null
**`zone_assets_pending`** note while the assets are missing, and `nav_state` reads `zone_loading`
until they land.

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
nullable reason naming *why* the roster is empty, mirroring
[`zone_map_load`](#zone_map_load--the-map-labeled-fallbacks-load-outcome-816); (2) a
server-advertised total to compare holdings against; or, failing both, (3) an explicit "no
completeness signal exists" field, so the endpoint discloses its own limit in band rather than only
in prose. **This is (3).** (1) is not built here because this client retains no record of whether
the packet that would separate "not sent yet" from "none exist" has arrived, so it has no reason to
name. (2) is not built here: this client compares its holdings against no total. For
`zone_entrances` the server does advertise one — EQEmu's `Client::SendZonePoints()` writes
`zp->count` (`zone/client.cpp:6959`) and the RoF2 patch copies it onto the wire
(`common/patches/rof2.cpp:3632`) — and since #1094 `apply_zone_points` **does** read it, but **only
to bound its parse**, never to judge the roster. It could not be compared naively even if something
wanted to: the #136 sentinel filter drops entries from inside the counted range, so `held` and the
count legitimately differ. Turning it into a completeness observable is #939's scope, not this
disclosure's.
`zone_map_load` stays what it is: a terminal fact about one *additive* contributor to
`zone_entrances` (#816), not a completeness verdict on any roster: it reports the outcome of that
`.txt` load and nothing else, so it says nothing about whether the server-advertised entries have
arrived.

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
positively confirm a record landed rather than infer it, the recourse is unchanged: re-list, or
packet capture, with the retroactivity caveat spelled out for doors under
[`zone_assets`](#zone_assets--is-the-world-this-response-describes-actually-loaded-579) above.

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

**The same reasons now reach `POST /v1/move/zone_cross` (#815).** That endpoint used to report
`zone_line_not_in_map` — documented as a map-data *gap* — when the region map had failed to load
rather than merely lacking the region, which is a claim about the contents of a file the client
never read. It now publishes the `region_data_*` reason above as its `nav_reason` instead, so the
two surfaces answer the same question with the same vocabulary. See the `zone_cross` reason table
under [Navigation state](#navigation-state).

**Now closed on `zone_cross` too (#827).** Until #827, a `zone_cross` reporting
`zone_line_not_in_map` could not be told apart from a collision slot that held **no grid at all** —
`zone_cross` asked the zone-asset state for permission and then read the collision slot under a
*second*, separate lock, and `begin_zone_load` empties that slot **before** it publishes `pending`,
so a zone change landing between the two reads paired a usable verdict with an emptied slot and the
lookup returned nothing for want of a grid. #829's reviewer constructed that end state directly and
got `no_path` / `zone_line_not_in_map` out of it. `zone_cross` now takes the verdict **and** the
grid from the one `usable_collision` call #821 introduced for `/v1/observe/zone_exits`: that call
hands back the `Arc<Collision>` the `ready` state owns, so the region lookup cannot be reached
without the grid the gate just vouched for and there is no longer an optional grid to be absent.
`zone_line_not_in_map` on `zone_cross` therefore means a region map that was read. (The third case,
a grid that is present but whose region data failed to load, is what #815 split out to
`region_data_*`.) Two limits, stated rather than implied: whether a real zone change actually
interleaves that way was never measured — the fix removes the pairing, so the question is moot
rather than answered; and this constrains `zone_cross`, not the other readers of the collision slot.

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

### Target-form precedence and `ignored_fields` (#901)

`POST /v1/move/goto`'s body accepts **four** ways to say where to go: a `name`, a complete
`{map_x,map_y}` pair, a complete raw `{x,y,z}` triple, or nothing at all (the current target). A
body that supplies more than one of these no longer picks a winner and silently drops the rest —
every loser is either **named in the response** or the whole request is **rejected**, depending on
which:

1. **`name` beside ANY coordinate field (complete or not) is a `400`.** Coordinates never
   disambiguate a name match — the fuzzy-match resolver never reads `x`/`y`/`z`/`map_x`/`map_y` —
   so a caller that sent both, believing the coordinates would narrow a duplicated name, is
   necessarily wrong to expect that. There is no reading under which the two belong in the same
   request, so this is rejected outright rather than reported:
   ```jsonc
   POST /v1/move/goto {"name": "a rat", "x": 10, "y": 20, "z": 3}
   400 "conflicting target: \"name\" (\"a rat\") and coordinate field(s) {x, y, z} were both
        provided — coordinates never disambiguate a name match, so send exactly one target form
        (a name, or coordinates, not both)"
   ```
2. **Between the two coordinate forms, a complete `{map_x,map_y}` pair beats a complete raw
   `{x,y,z}` triple.** Unlike (1), sending both is a plausible non-contradictory pattern (e.g. a
   caller that always populates a Brewall-derived AND a server-derived description of the same
   point), so the precedence still applies but the loser is now **named**, not dropped: a `200`
   response always carries an `ignored_fields` array listing every field the winning form didn't
   consume (empty when nothing was discarded). `z` is shared between the two forms (it overrides
   the map form's default z when present) and is never listed as ignored. This also covers the
   form the two confirmed cases above didn't name explicitly: a **lone** `map_x` or `map_y` (not a
   complete pair) beside a complete raw triple used to vanish with no trace at all — it is now
   named in `ignored_fields` too.
   ```jsonc
   POST /v1/move/goto {"map_x": 10, "map_y": 20, "x": 999, "y": 888, "z": 777}
   200 {"status": "navigating", "goal": [-10, -20, 777], "ignored_fields": ["x", "y"], ...}
   ```
3. **A body with SOME coordinate field(s) but not enough to complete either form** still 400s
   naming the missing field(s) — see the `/goto` row above (#886) — this is unchanged by #901.

> **Migration note:** before #901, case (1) above (`name` beside a coordinate field) answered
> `200` and silently routed to the name, dropping the coordinates with no trace. It now answers
> `400`. A caller that was relying on that silent routing (rather than sending one target form)
> must stop sending both fields in the same request.

### Mutually exclusive request forms (#952/#956)

The section above is `/goto`'s instance of a rule that now covers the whole API. Several routes
accept **more than one way to name the same argument**, and each of them used to resolve the choice
with a precedence chain that never looked at the losers. Every request struct carries
`serde(deny_unknown_fields)`, so a *misspelled* field is rejected. That is exactly what makes a
*declared but unreached* field quiet: it is not unknown, so it parses, and the request is answered
`200`. That is the failure this rule removes: the strongest signal the API can send ("your
instruction was understood") for an instruction that was thrown away.

> **#971 closed the last two gaps.** `GET /v1/observe/frame` and `GET /v1/observe/packets` were the
> only request structs without `deny_unknown_fields`; all 35 of this crate's request structs carry it
> now, so an unrecognized query key is **refused** rather than dropped — both driven:
>
> * `GET /v1/observe/packets?sicne=1` → `400 {"error":"invalid_query_param"}`, the message naming
>   `sicne` and listing the recognized keys. `/packets` gained the hand-rolled query parse `/frame`
>   already had, so this is JSON like every other error on the route, not axum's plain-text 400.
> * `GET /v1/observe/frame?allow_pending=1&prset=top_down&pitch=10` → `400
>   {"error":"invalid_query_param"}`. It used to answer `200` with an override built from the
>   surviving `pitch` **alone**, `azimuth` and `radius` left at their defaults — a PNG taken at an
>   angle nobody asked for, returned with the strongest "understood" signal the API has.
>
> **Breaking change:** a caller sending a key this API does not recognize now gets a `4xx` where it
> got a `200`. That is the point; the `200` was never evidence the key did anything. See also
> [Camera override for `/frame`](#camera-override-for-observeframe-422) above. The status codes are
> asserted by `req_form::tests::an_unrecognized_query_key_is_refused_on_both_observe_routes`, the now
> empty exception set by `every_deserialize_request_struct_is_classified`, and the `35` on this page
> against that module's constants by
> `docs_http_api_md_may_not_disagree_with_this_modules_struct_counts`. That test also holds the `8`
> and the `27` fixed relative to that figure, but neither is derived from the code — re-derive them
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
| `POST /v1/move/goto` | `name` vs any coordinate field | already `400` (#901) |
| `GET /v1/observe/frame` | `preset` vs `pitch`/`yaw`/`distance` | already `400` (#422) |
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
  it — all **35** — to be classified as exclusive or composable. So a **new request struct** cannot
  be added without that decision being made out loud.
* A **new field on an existing struct** is a different case, and only **8** of the 35 are protected
  against it: the eight handlers that destructure their body exhaustively (`let CommandBody { command,
  name } = &b;`, no `..`), where adding a field is a compile error until someone places it. On the
  other 27 structs, a new field that no code path reads would compile and ship — which is the
  original #952 shape. Re-derive both figures:
  `grep -rn '^#\[derive(.*Deserialize' crates/eqoxide-http/src/ | wc -l` and
  `grep -rnE '^\s*let [A-Za-z]+Body \{[^}]*\} = ' crates/eqoxide-http/src/*.rs`.
* The 26 `COMPOSABLE` rows carry a one-line reason each ("all three passed to `request_mem_spell`").
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

### `nav_goal_id` and `nav_goal` — goal identity (#349)

`GET /v1/observe/debug` carries two more fields. **They are top-level — siblings of `player`, not
inside it**, unlike `nav_state` and `nav_reason`, which are inside `player`. (Measured on a live
client while checking the rule below; the previous wording said "top-level fields under `player`",
which is not a place, and an agent that took it literally would read `null` forever.)

- **`nav_goal_id`** — a monotonically increasing counter, bumped every time a `POST /v1/move/{goto,follow,zone_cross,stop}` is accepted. It is **echoed in each of those POST's response bodies**: as a JSON `"goal_id": N` field on `/goto` and `/follow`, and as `[goal_id=N]` in the text body of `/stop` and `/zone_cross`. `nav_state`/`nav_reason` are the status *of this goal id* — never of an earlier one. **`/zone_cross` is the one route whose returned id does not end up carrying the outcome**: resolving the request into a concrete walk stamps a fresh, higher id (see below).
- **`nav_goal`** — that goal's `[x, y, z]` (server coords), or `null` for `idle`/`stop`, or for a `zone_cross` whose concrete zone-line destination the walker has not resolved yet. **`nav_goal` is `null` on every `idle`, whichever `nav_reason` got you there** — `zoned`, `goal_dropped`, `respawned`, `hp_restored`, `stopped`, `goto_superseded`, `zone_cross_dropped_unhandled` — because the coordinates are a fact about the goal and the goal is over (#732). The `zoned` case is the sharp one and the one that was actually wrong: coordinates are a **per-zone namespace** and carry no zone tag, so a goal that survived a crossing was a well-formed, numerically-plausible answer about the zone you just left. `nav_goal_id` deliberately does **not** reset — it is identity, not a per-goal fact, and it is what lets you match the `idle` to the request that produced it.

**Why this exists.** `POST /goto` returns `200` and sets the target, but the walker only re-labels `nav_state` on its next ~150 ms tick. Without identity, this canonical loop lied:

```
POST /v1/move/goto {...}   -> 200 {"goal_id": 8, ...}
GET  /v1/observe/debug     -> nav_state: "arrived"   <-- but nav_goal_id: 7, the PREVIOUS goto!
```

Now the accept **atomically** bumps `nav_goal_id` and resets `nav_state` to `pending`, so the read above returns `nav_state: "pending", nav_goal_id: 8` — honest.

**Rule: ignore any `nav_state` whose `nav_goal_id` is LOWER than the `goal_id` your POST returned — that is an older goal's outcome. At your id or above, the state is current.** A matching id with `pending`/`planning`/`navigating`/`navigating_partial`/`navigating_stalled`/`following` means your goal is genuinely in flight — `navigating_stalled` included: it is a walker that has a route and is not currently executing it, still recovering, not a verdict (see [`nav_stall`](#nav_stall--a-committed-route-the-walker-is-not-executing-851)) — and it will not stay that way, because any in-progress state with nothing behind it retires to `idle` with a reason on the next walker tick ([Why an in-progress `nav_state` can never stick](#why-an-in-progress-nav_state-can-never-stick-725)). `idle` **at or above your own goal id** is therefore an outcome, not a "not started yet": read `nav_reason`, which since #725 always says which outcome it was.

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

`POST /v1/move/zone_cross` reports further `no_path` reasons, all specific to zone-line crossing (#267):

| Reason | Meaning |
|--------|---------|
| `no_zone_line_to_zone` | The server never advertised (`OP_SendZonepoints`) any zone line from here to the requested `zone_id` — it will not appear in `/v1/observe/zone_exits` either. A genuinely invalid request: pick a `zone_id` that's actually one of this zone's exits. |
| `zone_line_not_in_map` | The requested `zone_id` **is** advertised by the server as a real exit, but the zone's region map **loaded** and has no matching WLD zone-line (DRNTP) trigger region for it — a client-side `.wtr` map-data gap, not proof the exit doesn't exist in the real game. Before reporting this, the client tries one fallback (#683): if the map has a zone-line region under some OTHER (unadvertised) index — e.g. an exit baked with index 0 — and this zone advertises no same-zone teleport pad and server zone points are available, it walks there instead and lets the server resolve the destination, so this reason now means no usable zone-line region exists at all (or the fallback is gated: a same-zone pad is advertised, or no server zone points are available). When the fallback **is** taken you do not have to infer it from the message log: `zone_cross_best_effort` on `/v1/observe/debug` says so structurally (#713, [below](#zone-cross-degradations-you-can-detect-713)). It is also omitted from `/v1/observe/zone_exits` (which only lists regions actually found in the loaded map), so "absent from `zone_exits`" does not by itself distinguish this from `no_zone_line_to_zone` — only `nav_reason` does. **"loaded" is now literal (#827):** this reason used to also be what you got when the client held **no collision grid at all**, because the permission check and the grid read came from two separate slots. `zone_cross` now takes both from one `usable_collision` call, so the grid it looks in is the grid the check blessed and there is no gridless path to this reason. Together with #815 (a present grid whose region data failed to load reports `region_data_*` instead, next row), this reason means: a region map was read, and it has no matching zone-line region. |
| `region_data_missing`, `region_data_unreadable`, `region_data_not_region_data`, `region_data_unsupported_version`, `region_data_truncated` | **The zone's region map (its `.wtr`) did not load, so the client does not know whether a zone line to `zone_id` exists** (#815). This is NOT `zone_line_not_in_map`: that reason is the *map-data gap* answer — "the map was read and the region is not in it" (previous row) — and publishing it for a file the client never read would be a definitive claim about contents it does not have. The cause strings are the same ones `/v1/observe/zone_exits` refuses with — [same table, same meanings](#zone-exits--means-exactly-one-thing-803). **This does not resolve itself by polling**, which is why the state is `no_path` rather than `zone_loading`: it is a missing/unusable asset, not a load in progress, and it applies to *every* exit in this zone, not just the one you asked for. Re-sync the zone's asset pack; `/v1/observe/zone_entrances` (server-advertised, independent of the `.wtr`) still works meanwhile. The sixth cause, `region_data_not_attached`, can reach this field by the same path and carries the same "a client bug, not an asset problem" reading given [above](#zone-exits--means-exactly-one-thing-803) — with the same caveat that no release build is believed to construct such a grid. |

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
| `fall_would_be_lethal` | The next waypoint is down a drop whose fall damage — **as this client computes and reports it** — reaches current HP. Stopped at the ledge. **This guard has a BOUNDED REACH and is silently inert outside it (#1058).** `fall_damage` clamps the impact velocity it derives, so the largest figure it can ever report is **774** (`eqoxide_core::physics::fall_damage_ceiling()` re-derives that number from the function rather than repeating it). The guard's comparison is `max_damage >= hp`, so at **775 HP or more it is unconditionally false and no drop of any size trips it** — the walker descends every ledge it is asked to. **Read the ABSENCE of this reason accordingly:** at those HP levels it means "the guard could not fire", never "the fall was weighed and found survivable", and the walker now logs the difference at the ledge instead of leaving the two indistinguishable — read it at `GET /v1/observe/messages?kind=zone`. Whether 774 is a real property of a fall or an artefact of a constant nobody has re-derived is **unresolved** — the damage curve is uncited, and the measurement that would settle it (validating the curve against the real client across drop heights, #1005) has not been run. Do not read this row as a promise of fall safety for a high-HP character. |
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
`goal_dropped`, `respawned`, `hp_restored`, `stopped`, `goto_superseded`,
`zone_cross_dropped_unhandled`. The fine
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

*Session-scoped* is the agent-facing name for that lifetime, and it is accurate today: the latch the
client keeps internally is scoped to the fine **worker**, and exactly one fine worker is built per
client process, so from out here the two are the same span. The distinction only matters to the
client's own code, which is where it is written down — together with a **tripwire, not a proof**,
under the one-worker premise this name rests on: a scan of the tracked tree that fails, naming this
paragraph, when a second fine-worker construction *site* is added in the ordinary way (#787). It
counts construction sites in source text, so it does **not** catch a second worker reached by a
function-pointer binding, an angle-bracket qualified path, a construction inside a `macro_rules!`
body that carries the guard's non-production marker, a site anyone marked as non-production
regardless of which file it is in, or — the one that matters most — **the same single site executed
twice**, which is exactly the in-process relogin shape. Every one of those gaps was measured, not
inferred. The scan's own reach is also weaker in environments without a git checkout, which the
guard now states on every run; the merge gate has one. Read "session-scoped" as accurate today and
cheaply checked, not as guaranteed.

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
cannot be sent, so the shutdown cannot end the session with a clean camp-out (#890). (Counted from
the route table: 58 write routes, of which **4** skip the guard. The other three are not exceptions
to the sentence above, because none of them reaches the server: `POST /v1/camera` and
`/v1/camera/reset`, drained by the render thread, and `/v1/social/friends`, which is documented
client-local and sends no packet.)

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

### `zone_map_load` — the map-labeled fallback's load outcome (#816)

Top-level on `GET /v1/observe/debug`. `zone_entrances` (and its deprecated alias `zone_points`)
carries two kinds of entries: the server-advertised ones (`OP_SendZonepoints`), and a handful of
**client-synthesized** entries the client reads from the CURRENT zone's own map `.txt` pack (base
file plus its optional `_1`/`_2`/`_3` detail layers). The label heuristic only ever recognizes a
`"to "`-prefixed label naming North Qeynos, South Qeynos, or Qeynos2 — i.e. it only ever synthesizes
an entry whose destination `zone_id` is 1 or 2. That is a property of the label TEXT, not of which
zone you are standing in: any zone whose own map pack happens to contain such a label contributes.

**Measured, not assumed** (round 2 of #816's review found the previous wording — "only North/South
Qeynos" — read as "only those two zones contribute", which is false). Method: `ZoneMap::try_load`
plus this exact matching heuristic, run for real (not re-derived by reading the code) over every
base `.txt` pack in the real shipped maps directory (`~/.local/share/eqoxide/assets/models/maps`,
526 base packs found, reach-controlled at >400 scanned as an integrity check against silent
early-exit). Five zones' own packs carry at least one qualifying label — the number in parentheses
is how many: `erudsxing` (2), `qcat` (13), `qeynos` (4), `qeynos2` (4), `qeytoqrg` (1). Every other
zone's map, if it has one, contributes nothing — not a special case, just that no other shipped pack
happens to contain matching text. Two of the five (`erudsxing`, `qeytoqrg`) carry 100% of their
qualifying labels in a `_1.txt` detail layer and 0% in the base file — the detail-layer read matters
for real data, not just in principle (see the `zone_map_layer_unreadable` bullet below).

That `.txt` read (base or a detail layer) can fail (no file for this zone, a permissions error, a
directory in its place), and when it does, those fallback entries are simply absent from
`zone_entrances` rather than announced as missing — the exact silent-omission shape the
agent-honesty invariant forbids.

`zone_map_load` names the outcome instead of hiding it:

- `null` — either this zone's map (base file, and every detail layer that exists) loaded fine and
  `zone_entrances` is carrying every fallback entry it has to offer, OR the zone has not changed yet
  this session and no load has been attempted, in which case it is carrying none of them. Cross-check
  `zone` and `zone_assets` if you need to tell the two apart.
- `{"reason": "zone_map_missing", ...}` — there is no base map `.txt` for this zone at all. Measured:
  the shipped pack contains 526 base `.txt` maps, covering essentially every zone a character plays in,
  so this is NOT the ordinary reading for a normal zone — the ordinary reading for a zone with a map
  and no qualifying label is `null`. Treat `zone_map_missing` as "this client's maps cache does not
  have this zone", i.e. usually an incomplete asset sync, and only harmless once you have confirmed
  you were not relying on this zone's synthesized entries.
- `{"reason": "zone_map_unreadable", "detail": "..."}` — the base file is present but could not be
  read (a permissions error, a directory sitting where the file should be, a corrupt mount).
  Distinct from `zone_map_missing` on purpose: "confirmed absent" and "present but unreadable" are
  different diagnoses and must not collapse into one value.
- `{"reason": "zone_map_layer_unreadable", "detail": "..."}` — the base file loaded fine, but a
  `_1`/`_2`/`_3.txt` detail layer that IS present could not be read. Added in #816 round 2: this
  used to be swallowed silently one level below the base-file case this whole field exists to
  fix — a present-but-unreadable detail layer read as a healthy `null` with whatever the base file
  alone contributed, which is the identical confident-but-wrong shape for a zone like `erudsxing` or
  `qeytoqrg` whose ENTIRE qualifying label set lives in a layer, not the base file. It is now a
  distinct, named, non-null outcome instead.
  **The base file's OWN entries are forfeited too, not just the broken layer's** (#873): `try_load`
  fails the WHOLE load, so `zone_entrances` gets none of this zone's map-labeled fallback entries at
  all — for `qcat`, an unreadable `_1.txt` takes the zone from **13 qualifying labels** (6 in the
  base file, 7 in `_1.txt`, 0 in `_2.txt`; measured over the shipped map pack) to **0**, not down to
  the 6 the base file alone would have supplied. Those 13 are *labels*, not entries: `sync_zone_points`
  dedups each qualifying label against the zone points the server already advertised (same `zone_id`,
  within 50 units) before any of them becomes an entry, so the number of `zone_entrances` entries
  actually forfeited is **≤13 and has not been measured** — it needs a live session. What is exact is
  the direction: whatever the base file would have contributed, an unreadable layer takes it to zero.
  This is deliberate, the same reasoning as the rest of this bullet: a half-map that reads as "the
  load worked" is the exact failure #816 is about, and the base file's entries loading fine in
  isolation gives no guarantee they're the zone's
  COMPLETE fallback set once a layer next to them is known to be broken. Treat `zone_map_load` being
  non-null as "assume none of `zone_entrances`' map-derived fallback entries for this zone are
  present", not "assume only the broken layer's entries are missing".

**This is deliberately NOT a 503 gate**, unlike `region_data_missing` et al. on `/v1/observe/zone_exits`
(#815). The reasoning differs from that case: `zone_exits` derives its verdict entirely from the
`.wtr` region map, so a failed `.wtr` read leaves it with nothing truthful to say about ANY exit.
`zone_entrances`'s primary content — server-advertised zone points — is completely unaffected by a
`.txt` load failure; only the small, zone-specific, purely-additive fallback contribution is in
question. Refusing the whole endpoint over that would be a strictly worse answer than serving what
IS known and disclosing the gap here.

Recorded fresh on every zone change (success included), so a failure from a PREVIOUS zone can never
survive, stale, into a zone whose map load actually succeeded.

**Not just a reporting gap.** `zone_entrances` and the in-memory list it reads
(`world.zone_points`) are the same `Arc<Mutex<Vec<ZonePoint>>>` two other HTTP-observable code
paths consult — confirmed by reading all three call sites, not independently confirmed on a live
client this round (see below): `POST /v1/move/zone_cross`'s reachable-`zone_id` check
(`crates/eqoxide-http/src/move_api.rs`, `reachable_zone_ids`) rejects a `zone_id` with 400 if it is
not in this same list, and the walker's own zone-cross resolution
(`crates/eqoxide-net/src/action_loop.rs`) reports `nav_reason: "no_zone_line_to_zone"` when a
requested destination isn't found in it. A load gap that shortens `zone_entrances` shortens the
list both of those consult too — the practical effect of a swallowed layer error is not only a
shorter HTTP report, it is a real 400 or a real `no_path` for a crossing that a fully-loaded map
would have allowed. **Not measured live this round**: an attempt to launch a client to confirm this
end-to-end (e.g. a `POST /v1/move/zone_cross` for a Qeynos-adjacent zone_id from inside `qcat`) was
blocked by this environment's own client-launch guard; the shared-field claim above rests on reading
`crates/eqoxide-http/src/move_api.rs:392`, `crates/eqoxide-net/src/action_loop.rs:1745`, and
`crates/eqoxide-http/src/observe.rs`'s `get_zone_entrances`, not on an observed wire response.
Widening the #816 fix itself was not needed for this: `ZoneMap::try_load` (this PR's fix target) is
the single source both of those call sites' data flows through, so the code fix already covers them;
what changed here is only that the docs and PR body now say so.

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
- **`clearance`** — a throttled live sample of nav's own traversability model (which may lag the
  player a few ticks). Fields:

  ```json
  "clearance": {
    "at": [128.5, -64.0],
    "anchor": {"kind": "floor", "z": -10.0, "reference_z": -11.0},
    "body": "footprint_pierced",
    "wall_spokes": ["clear_to_cap", {"hit": {"at": 2.75}}, "..."],
    "cap": 4.0,
    "footprint_ok": [true, true, false, "..."],
    "footprint_radius": 1.0,
    "footprint_ring_z": -7.0,
    "field_wall": 3.0,
    "field_ground": 2.0
  }
  ```

  - **`at`** — the horizontal `[east, north]` the sample was taken at. **Two elements, not three:**
    the vertical is no longer here, because there is more than one of them (see `anchor`).
  - **`anchor`** — WHERE the vertical came from, internally tagged on `kind`:
    - `{"kind": "floor", "z": …, "reference_z": …}` — a floor was found in the search band and the
      rays were cast from `z`;
    - `{"kind": "no_floor_in_band", "reference_z": …}` — **no floor was found**, and the rays were
      cast from `reference_z`. There is **no `z` key at all** on this variant; a consumer must
      branch on `kind` before reaching for one.

    `reference_z` is the character's own height and is present on both variants. It is not always
    equal to `z`: a body embedded under a slab has its nearest floor *above* it, so the rest of the
    sample can describe a point in open air over the geometry the character is inside.
  - **`body`** — the movement controller's placement verdict **at the character's own position**:
    `placeable` | `footprint_pierced` | `no_floor_below` | `footprint_pierced_and_no_floor_below`.
    Anything but `placeable` means the rest of this sample describes a point the character does not
    occupy. It is **not** a claim about whether the character can move — a non-`placeable` verdict
    is the entry condition to the depenetration net, which usually relocates the body and it keeps
    moving. The published answer to "can it move" is `player.hold` on `/v1/observe/debug`.
  - **`wall_spokes`** — 16 radial readings, each either the string `"clear_to_cap"` (nothing was hit
    anywhere within `cap` — a **lower bound**, not a distance of `cap`) or `{"hit": {"at": <f32>}}`
    (a measured distance in `0 ..= cap`).
  - **`cap`**, **`footprint_ok`** (8-direction ring), **`footprint_radius`**, **`footprint_ring_z`**
    (the z the planner's ring was tested at — the anchor plus the body's ring offset), and the
    zone-lifetime field values **`field_wall`** / **`field_ground`** the planner's hug-cost and
    margin actually consult.

  > **Breaking change (#885)** for any consumer written against the previous shape:
  > `at` went from **3 elements to 2**; `wall_spokes[i]` went from a bare **float** to the tagged
  > union above, so `as_f64()` on a saturated spoke now returns `None` rather than `cap`; and
  > `anchor` / `body` / `footprint_ring_z` are new. The old encoding wrote `cap` for both "hit at
  > exactly the cap" and "measured nothing", and wrote the fallback `reference_z` into `at[2]` under
  > a field documented as a floor height — both are why the shape changed.
- **`water`** — the swim state the walker acted on this tick (`swimming`, `swim_plane`), i.e. the
  values that went into its MoveIntent — not a recomputation.

---

## Nav footing verification (`nav_support`)

`GET /v1/observe/debug` also carries **`nav_support`** (top-level, not under `player`) — whether pathing in the current zone is
answering from **winding-blind (inverted-art) ground**. **`null` means every standable surface so far
faced UP** (properly wound); an object means nav has answered from a down-facing surface:

```json
"nav_support": {
  "reason":   "facing_blind_ground",
  "surfaces": 412,
  "detail":   "parts of this zone's collision mesh are wound INVERTED ..."
}
```

Since **D-2 (#375)** nav's floor predicate `is_standable` is **facing-blind**: a surface is ground on
its flatness + headroom, whichever way its art is wound — because some zones bake real, walkable
ground from **inverted (down-facing) art** (the qcat live wedge stood on exactly such a walkway, which
the old up-facing-only filter deleted). Those surfaces ARE walkable, but nav can no longer *verify*
their facing, so `nav_support` reports that it has been standing on some.

> **Renamed from `nav_degraded`/`inverted_floor_art`.** That older signal counted a `column_bottom`
> recovery valve, which D-2 removed. Had it been left reading the dead counter it would report `null`
> ("all pathing on properly-wound floors") in exactly the inverted-art zones (permafrost/highpass/
> neriakc/qcat) where nav is now on winding-blind ground — a confident falsehood. The signal moved
> with the mechanism so it stays honest.

> **Breaking change (#960): the count field is `surfaces`; it used to be spelled `queries`.** It never
> was a per-request count, so the old spelling stated a quantity the client does not measure. A
> consumer keyed on the old name now gets `undefined`/`None` rather than a plausible wrong number.

**`surfaces` is an unscaled total, not a rate.** It advances once per **down-facing triangle** nav
admits as standing ground, per call — so one ground probe over a column carrying two such triangles
adds **2**, and the value scales with how the zone's art happens to be tessellated. Do not derive a
frequency from it, and do not compare it across zones. Read it as "how much winding-blind ground nav
has leaned on since zone load".

Read `nav_support != null` as *"footing here is unverified-winding"* — not an error and not a
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

## `skin_cap_downgrades` — the renderer's silent render-arm downgrades, made visible (#797/#848)

The renderer has an animation joint cap. A character model whose skin exceeds it is not refused —
it is silently switched to the **static (unskinned) render arm**: it still appears on screen, in the
right place, but it will never animate again for the rest of the session. Before #797 there was no
observable for this at all — the client logged an error server-side of the agent's view and moved
on. An agent with no eyes on the screen had no way to learn that what it believes is a walking,
attacking character is actually a frozen pose, short of a human telling it so.

`GET /v1/observe/debug` carries it as a top-level object, keyed by the **base file name** of the
model that was loaded (never a full path — see below):

```jsonc
"skin_cap_downgrades": {
  "race_hum.glb": { "joint_count": 190, "key_collision": false }
}
```

- **`{}` (never `null`, never omitted) when no downgrade has been recorded yet this session.** Like
  `nav_local_planner_dead`, this field is *always present* rather than `null`-when-healthy, so an
  agent that greps this response and finds the key missing entirely knows it is talking to a client
  too old to report this, rather than concluding every model in the zone animates fine.
  **Read `{}` precisely.** It means "nothing has downgraded so far", which covers two states this
  field cannot separate: *every character model loaded so far fits the cap*, and *no character
  model has been loaded yet* — a loading screen, a zone-in still in progress, or simply nothing in
  view. `{}` is not evidence that a particular model animates; it is only evidence that no model
  the renderer has actually loaded has been recorded as downgraded. What `{}` does **not** hide is
  a stale non-empty state: the map's only writer runs inside `render_frame`, the publish is the
  next statement after that same call, and the map is only ever inserted into or updated (never
  cleared), so a downgrade that has happened is visible on the very next response.
- **`joint_count`** — the joint count that triggered the downgrade. If the same key has been (re)loaded
  more than once this session, this is the **most recent** load's count, not the first.
- **`key_collision`** — see below. `false` for the overwhelming common case (one file, one key).

### Why the key is a base name, not a path — and what that costs (#848)

The renderer never loads two files from the same directory with the same base name for one race —
gender variants and equipment-driven swaps all produce distinct base names — so a base-name key
almost always identifies one real file uniquely, without ever putting a local filesystem path in an
agent-facing response (this project does not publish local paths over the API).

The cost is that if two *different* asset roots ever legitimately produce two different files that
share a base name (a custom asset override alongside the stock one, for instance), they collide onto
the same map entry. Before #848 that collision was silent: the second load's joint count quietly
overwrote the first's, with nothing in the response saying two distinct files had ever shared the
key — a caller who trusted the entry would have been reading a number that belonged to *either* file,
with no way to tell which, or that there was even a question to ask.

`key_collision: true` is what makes that detectable. It is **sticky**: once two source files with
different identities have ever both written the same key in a session, it stays `true` for that key
for the rest of the session, even if every write after that agrees with the one before it (loading
the same colliding file twice in a row does not clear it — the collision already happened and the
entry is still unable to say which file `joint_count` currently describes). Treat `key_collision:
true` as "this entry's `joint_count` is not reliably attributable to one file" — not as "the count
shown is wrong," and not as something that self-heals.

Both fields are keyed off the path `ModelAsset::load` itself actually opened, rather than a
caller-supplied path/label pair. That path is stored in a field private to the renderer's
`models.rs` and readable only through `ModelAsset::loaded_from()`, so no code upstream of the
renderer — nor anywhere else in the client outside that one module — can either construct a
`ModelAsset` carrying a path of its choosing or overwrite the one `load` recorded. Both routes are
compile errors, and both were tried: removing the old caller-supplied argument alone was *not*
enough (with the field still public, an assignment at the call site compiled clean and filed every
downgrade under the wrong name), which is why the field is private rather than merely unused.

What this does **not** promise: that the file `load` opened is the right file for the character you
are looking at. The key is faithful to what was loaded; choosing what to load is a separate
question, and this field cannot answer it.
