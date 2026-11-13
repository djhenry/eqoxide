# eqoxide Agent Plugin API

eqoxide's only programmatic interface: a persistent duplex Unix-socket connection carrying two
message shapes, `Step` in and `Observation` out, at a fixed tick cadence. A `Step` carries
continuous movement, at most one discrete **verb**, and any number of one-shot **calls**; the
`Observation` carries the character's state plus the answers to earlier verbs (`verb_results`) and
calls (`call_replies`). Everything an agent can do or read is reachable this way: per-tick actions
are verbs, everything else is a call to an [agent op](agent-ops.md).

Related documents: `docs/specs/2026-09-15-agent-plugin-api-design.md` (original design),
`docs/specs/2026-11-13-agent-api-verbs-and-calls-design.md` (how verbs, calls and answers fit
together, version history, deferrals) and `docs/specs/2026-09-21-agent-harness-separation-design.md`
(why this socket is the sole interface). This document is the wire reference for a consumer
implementing a client against it.
[`eqoxide-agent-harness-example-http-astar`](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar)
is a reference harness: an HTTP front-end, A* navigation and chase-and-engage combat positioning
built purely on `Step`/`Observation`/`Call`.

The wire types live in the `eqoxide-agent-protocol` crate, which has **zero dependency on any other
eqoxide crate** — it's what an external agent project pins directly. The in-client server lives in
`eqoxide-agent-plugin-host`; the call handlers live in `eqoxide-agent-ops`.

## Enabling it

The API is **off by default**. Pass `--agent-socket <PATH>` on the eqoxide command line to bind it
to a Unix domain socket at that path:

```
eqoxide --agent-socket /tmp/eqoxide-agent.sock
```

There is no default path and no auto-scanned port — an unauthenticated local control-plane socket
must be something you opt into, not something every launch exposes. The socket file is created with
`0600` permissions (readable/writable only by the user that launched eqoxide); if a stale file
already exists at that path, eqoxide removes it and logs a warning before binding.

## Connecting

The socket serves **one connection at a time** — right for a single-character session. Connecting
while another client is attached simply queues behind it at the OS accept-queue level; there's no
separate multiplexing.

### Framing

Every message, in both directions, is one line of JSON terminated by `\n` (NDJSON) —
`eqoxide_agent_protocol::framing::encode_line` / `decode_line`. There is no length prefix; a reader
that splits on `\n` (e.g. `BufRead::read_line`, tokio's `AsyncBufReadExt::lines()`) is sufficient.

### Handshake

The client sends `Hello` first, always:

```json
{"protocol_version": 5}
```

The server replies with one `HandshakeReply` line:

```json
{"status": "Accepted", "asset_cache_dir": "/home/me/.local/share/eqoxide/assets/models"}
```

`asset_cache_dir` is the filesystem path of eqoxide's assembled asset directory
(`CacheDirs::models_dir()`); a client on the same machine reads zone GLBs and other zone files
directly from it. It is always present on `Accepted` (since protocol 2).

Or, on a version mismatch or a malformed `Hello`:

```json
{"status": "Rejected", "server_protocol_version": 5, "message": "client protocol_version 4 != server 5"}
```

(A malformed `Hello` is answered with `"message": "malformed Hello"`.) On `Rejected`, or on any
handshake failure (malformed line, EOF, or a 5-second timeout waiting for `Hello`), the server
closes the connection immediately — reconnect and try again rather than sending anything further on
that socket.

#### Versioning and backward compatibility

The version check is exact equality: there is no negotiation and no range. `PROTOCOL_VERSION`
(currently `5`, in `eqoxide_agent_protocol::handshake`) bumps whenever a wire type changes shape in
a way that breaks an old client *or an old host*. Pin the version you were built against and import
the constant rather than hard-coding it.

| Version | Added |
|---|---|
| 1 | Initial: `Step`, `Observation`, the combat/interact/lifecycle verbs. |
| 2 | `asset_cache_dir` on `Accepted`. |
| 3 | `Step.seq` and `Observation.verb_results` (`VerbResult`, `VerbOutcome`). |
| 4 | `Step.calls` and `Observation.call_replies` (`Call`, `CallReply`, `CallStatus`). |
| 5 | The verb set is part of the version: adds `Chat`, `Lifecycle::Camp/Exit`, `Inventory::Move/Destroy/Consume`, `Pet::Command`; removes `Merchant`, `Quests` and `Move`. |

The verb set is part of the version: any change to `AgentVerb` (a verb added or removed) bumps
`PROTOCOL_VERSION`, so a client never sends a verb that an older host would silently drop.

`Step` is `deny_unknown_fields`, so a host older than a field it receives would drop the whole line,
movement included, with no signal to the client. That is why every field addition to `Step` bumps the
version instead of being "additive": the mismatch becomes a loud `Rejected` at connect time rather
than a silent misparse. A client built for another version must be rebuilt against the current
`eqoxide-agent-protocol`.

Once handshake succeeds, the server begins the tick loop described below. Nothing about the
connection is itself session state — the character/game state is the one shared, persistent thing,
so a disconnect-and-reconnect resumes against the same running character. Verbs still queued and
calls still running die with the connection.

## Sending `Step`

At any point after a successful handshake, send zero or more `Step` lines:

```rust
pub struct Step {
    pub movement: Option<AgentMovement>,
    pub verb: Option<AgentVerb>,
    pub seq: Option<u64>,   // absent on the wire when None
    pub calls: Vec<Call>,   // absent on the wire when empty
}
```

`movement`, `verb` and `calls` are independent: a `Step` can carry any combination, or none. A
`Step` with `movement: None` does **not** clear movement an earlier `Step` latched. There is no
direct reply to a `Step`; the answers to its verb and calls come back inside later `Observation`s
(see [Answers](#answers-verb_results-and-call_replies)).

### `movement` — continuous, held-key-style

```rust
pub struct AgentMovement {
    pub dir: [f32; 2],           // world (east, north); any magnitude — NOT normalized server-side
    pub up: f32,                 // -1..1, only affects swimming/climbing (auto-detected, not agent-selected)
    pub jump: bool,
    pub wish_heading: Option<f32>,  // facing override; None = derive heading from `dir`
}
```

`dir = [0.0, 0.0]` means "stand in place" (useful for a jump-with-no-movement `Step`). eqoxide does
**not** normalize `dir` for you — send a unit vector if that's what you want applied.

The host latches the last `Step` with non-`None` movement and **re-applies it every tick for the
rest of the connection** — the same "held key" semantics as `ManualMove` elsewhere in the client.
It does not lapse when the agent goes quiet: to stop, send a `Step` with `dir = [0.0, 0.0]`. What
`MOVE_LATCH` (300 ms, 2x the tick period) bounds is each individual application: every tick the
host re-issues the move with a 300 ms deadline, so if the host stalls, or the connection ends,
the character stops within ~300 ms instead of walking on. A new connection starts with nothing
latched.

`wish_heading` exists because melee requires facing the target server-side; it can be set
independently of `dir`, e.g. to strafe sideways while still facing a target. A movement with a
non-finite number (`NaN`, overflow) is dropped; the `Step`'s verb and calls still run.

### `verb` — discrete, one-shot

```rust
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Inventory(InventoryVerb),
    Chat(ChatVerb),
    Pet(PetVerb),
}
```

A verb fires **exactly once**. Verbs read off the socket join a per-connection FIFO queue and the
tick loop dispatches **one verb per tick, oldest first**, so two verbs sent within one tick are
neither lost nor merged: the second runs 150 ms later. The queue holds at most 16 verbs
(`MAX_QUEUED_VERBS`); a verb arriving to a full queue is dropped and answered `Refused`.

| Variant | JSON shape | Dispatches to |
|---|---|---|
| `Combat::Target { spawn_id }` | `{"verb":"Combat","data":{"action":"Target","spawn_id":42}}` | `CommandState::request_target` |
| `Combat::Attack { on }` | `{"verb":"Combat","data":{"action":"Attack","on":true}}` | `CommandState::request_attack` |
| `Combat::Consider { spawn_id }` | `{"verb":"Combat","data":{"action":"Consider","spawn_id":7}}` | `CommandState::request_consider` |
| `Combat::Cast(CastRequest)` | `{"verb":"Combat","data":{"action":"Cast","gem":0,"target_id":42,"item_slot":null}}` | `CommandState::request_cast`; refused when `gem` > 8 |
| `Interact::Sit` | `{"verb":"Interact","data":{"action":"Sit"}}` | `request_sit(true)` |
| `Interact::Stand` | `{"verb":"Interact","data":{"action":"Stand"}}` | `request_sit(false)` |
| `Lifecycle::Respawn` | `{"verb":"Lifecycle","data":{"action":"Respawn"}}` | `request_respawn()`; refused when the character is not confirmed dead (`Observation.dead` is false) |
| `Lifecycle::Camp` | `{"verb":"Lifecycle","data":{"action":"Camp"}}` | `request_camp(Toggle)` |
| `Lifecycle::Exit` | `{"verb":"Lifecycle","data":{"action":"Exit"}}` | `request_camp(Start)` plus a 45 s watchdog that force-exits the process with a labelled reason; honoured even with a dead network thread |
| `Chat::Tell { to, text }` | `{"verb":"Chat","data":{"action":"Tell","to":"Alpha","text":"hi"}}` | `request_chat_send` on chan 7; refused when `to` or `text` is blank |
| `Chat::Ooc/Shout/Group/Guild { text }` | `{"verb":"Chat","data":{"action":"Ooc","text":"hi"}}` | `request_chat_send` on chan 5/3/2/0; refused when `text` is blank |
| `Inventory::Move { from, to }` | `{"verb":"Inventory","data":{"action":"Move","from":23,"to":33}}` | `request_inventory_move`; refused when `from` holds no item in the last published inventory, or a move is already queued. Slots are wire ids: 0-21 worn, 23-32 general, 33 cursor, 251+ bag contents. Equip and unequip are moves. |
| `Inventory::Destroy { slot }` | `{"verb":"Inventory","data":{"action":"Destroy","slot":24}}` | `request_inventory_destroy`; picks the item up to the cursor, then deletes it (one delete per unit of a stack). Refused when the character is dead, `slot` holds no item, the cursor already holds an item (unless `slot` is the cursor), `slot` is a bag that still holds items, or a destroy is already queued. Irreversible. |
| `Inventory::Consume { slot }` | `{"verb":"Inventory","data":{"action":"Consume","slot":25}}` | `request_inventory_consume`; eats or drinks one unit. Refused when the character is dead, `slot` holds no item, the item is not food or drink, or a consume is already queued. The server ignores a consume when the character is not hungry or thirsty; that is not visible to the host, so `Accepted` means sent, not eaten. Watch the inventory and `Observation` for the effect. |
| `Pet::Command { command }` | `{"verb":"Pet","data":{"action":"Command","command":2}}` | `request_pet_command` with a raw `PET_*` byte (2 attack, 4 follow, 5 guard, 6 sit toggle, 28 back off); refused when a command is already queued |

`CastRequest` mirrors `eqoxide_ipc::CastRequest`: `{ gem: u8, target_id: Option<u32>, item_slot:
Option<u32> }`.

Vendor and quest work, and zone crossing, have no verbs: a `Step` whose verb is tagged `Merchant`,
`Quests` or `Move` is malformed (rejected like any other bad line, connection kept). Vendor and quest
work are [calls](#calls); to cross a zone, walk to a zone line listed by
`GET /v1/observe/zone_entrances` / `zone_exits`.

Every verb is also refused while the network thread is dead (`"the network thread is not running:
..."`), except `Lifecycle::Exit`, whose job is tearing that session down.

### Which actions are verbs and which are calls

A **verb** is an action issued on the control-loop cadence that completes by being handed to the
game: target, attack, cast, sit, a chat line, an inventory move, destroy or consume, a pet command, respawn, camp. It is
one typed enum value, validated and answered within a tick. A **call** is everything else, and is
the right shape whenever the operation needs a *reply body* (reads: inventory, quest log, merchant
wares, rosters, the chat log, events) or can *take seconds* (a merchant purchase waits for the
server's echo; a turn-in waits for the NPC). Calls run on their own task, so a slow one never stalls
the tick loop or the verbs behind it. This is why vendor and quest work have no verbs: the whole
groups are calls.

| Group | Verbs | Calls (full list and bodies in [agent-ops.md](agent-ops.md)) |
|---|---|---|
| combat | `Combat::*` | `POST /v1/combat/memorize`, `POST /v1/combat/scribe` |
| interact | `Interact::Sit/Stand` | `POST /v1/interact/` `hail`, `say`, `loot`, `give`, `click_door`, `read`, `dialogue`, `run`, `walk` |
| lifecycle | `Lifecycle::Respawn/Camp/Exit` | none |
| inventory | `Inventory::Move` (also equips/unequips), `Destroy`, `Consume` | read: `GET /v1/observe/inventory` |
| chat | `Chat::Tell/Ooc/Shout/Group/Guild` | read: `GET /v1/observe/messages`, `GET /v1/events/all`, `GET /v1/events/<category>` |
| pet | `Pet::Command` | none |
| merchant | none | `POST /v1/merchant/` `open`, `close`, `buy`, `sell`; `GET /v1/merchant/list` |
| quests | none | `GET /v1/quests/` `log`, `completed`, `offers`; `POST /v1/quests/` `accept`, `decline`, `cancel` |
| trainer, group, guild, social | none | `GET`/`POST` under `/v1/trainer/`, `/v1/group/`, `/v1/guild/`, `/v1/social/friends` |
| observe | none | `GET /v1/observe/` `inventory`, `messages`, `dialogue`, `spells`, `skills`, `doors`, `zone_entrances`, `zone_points`, `zone_exits`, `item_text`, `who` |

### Malformed input

A `Step` line that fails to parse (an unknown field or an unknown verb) rejects just that one `Step`, movement included — the connection stays
open and nothing is sent back, because no answer exists for a line that never became a `Step`. A
line that exceeds 256KB with no terminating `\n` is treated as a client bad enough to disconnect
outright (the same cap applies to the handshake's `Hello` line).

### Calls

A `Call` asks eqoxide for something that is not movement or a verb:

```rust
pub struct Call {
    pub id: u64,          // caller-chosen, echoed in the CallReply; unique among your calls in flight
    pub op: String,       // "<METHOD> <path>", e.g. "GET /v1/observe/inventory"
    pub args: Value,      // omitted when null
}
```

```json
{"movement":null,"verb":null,"calls":[
  {"id":7,"op":"GET /v1/observe/messages","args":{"query":{"kind":"npc"}}},
  {"id":8,"op":"POST /v1/merchant/buy","args":{"body":{"merchant":"Bob","slot":3}}}]}
```

- **`op`** names a native op registered in `eqoxide-agent-ops` (`GET /v1/...` / `POST /v1/...`; the
  names and what each takes and returns are in [agent-ops.md](agent-ops.md)). The names are
  identifiers, not URLs: there is no HTTP server in eqoxide.
- **`args`** is an object with two optional keys: `query`, an object of query parameters, and
  `body`, the op's JSON request body. Either may be absent or null; anything else under `query`
  (not an object) is answered `Invalid`. Other keys are ignored.
- Calls in one `Step` start concurrently, and each runs to completion on its own task, so replies
  arrive **in the order calls finish, not the order sent**. Match by `id`.
- At most 32 calls may be running at once (`MAX_CALLS_IN_FLIGHT`); a call past that is not started
  and is answered `Unavailable` at once. The host imposes no deadline of its own on a call; each op
  bounds itself (merchant ops wait up to 4 s for the server's echo, `social` and some `observe`
  ops 6 s, `interact/give` 8 s, and a mutating call is tracked by an action watchdog that records a
  terminal outcome in the event feed after about 30 s even if the caller stopped waiting).

`CallStatus` says what kind of answer a call got. The status is derived from the op's reply code:
a refusal is never reported as `Ok`.

| Status | Meaning | From |
|---|---|---|
| `Ok` | The operation happened: a read returned its data, or a write was done and settled. | 2xx |
| `Accepted` | Handed to the game, which has not confirmed it; later reads show what the server did. | 202 |
| `Invalid` | Malformed request, or not possible in the current state. Retrying the same request won't help. | other 4xx |
| `NotFound` | The thing named (an item, spawn, quest, **or the op itself**) does not exist. | 404 |
| `Conflict` | Valid, but conflicts with something in flight or the game's state. Nothing was done; retry later may work. | 409 |
| `Unavailable` | The game cannot act now (no live session, assets not loaded), or the in-flight cap was hit. Nothing was done. | 503 |
| `Failed` | Anything else; the body says what. | other |

## Receiving `Observation`

The server pushes one `Observation` line **every tick, unconditionally** — independent of whether a
`Step` arrived that tick. There is no throttling or backpressure-driven skip; if you stop reading,
the socket write blocks and, after 450 ms (3 ticks), the server gives up and closes the connection
rather than wedging forever.

```rust
pub struct Observation {
    pub own: OwnState,
    pub visible: Vec<VisibleEntity>,
    pub legal_actions: LegalActionMask,
    pub dead: bool,
    pub terminated: bool,
    pub truncated: bool,
    pub visibility_available: bool,
    pub tick: u64,
    pub verb_results: Vec<VerbResult>,   // absent on the wire when empty
    pub call_replies: Vec<CallReply>,    // absent on the wire when empty
}
```

The `Observation` is deliberately lean: it holds the *state of the character and its surroundings*,
and nothing bulky. Inventory contents, merchant wares, quest journals, rosters and the chat/event log
are **not** pushed every 150 ms; read them on demand with a [call](#calls). That keeps the per-tick
line small and fast, and it is the same information a player gets: they open the bag, the vendor
window or the quest journal when they want to look.

- **`tick`** — starts at 0 on the first `Observation` after handshake, increments once per loop
  iteration that reached the point of building an `Observation` (not per successful write — a
  failed write ends the connection instead of retrying). Use it to detect dropped/bursted delivery
  and reconstruct elapsed time.
- **`dead`** — the character is dead as ongoing *state*, not a terminal boundary. This is routine in
  combat training: the agent keeps receiving observations while dead and must send
  `Lifecycle::Respawn` to continue.
- **`terminated`** — true session end (the net thread has died for good — e.g. disconnected from
  the EQ server). eqoxide only ever asserts this when it can back it up.
- **`truncated`** — always `false` from eqoxide's side; reserved for an external harness that
  wants to impose its own rollout cutoff without needing eqoxide to say so.
- **`visibility_available`** — false whenever no zone collision geometry is loaded yet (startup,
  zone transitions, or the synthetic debug zone `--testzone` builds). In that window `visible` is
  always `[]` regardless of what's actually nearby — that's *not* the same claim as "nothing is
  visible," so check this before trusting an empty `visible` list.
- **`verb_results`**, **`call_replies`** — the answers; see the next section.

### Answers: `verb_results` and `call_replies`

Every `Step` is unanswered on the wire; answers ride in the next `Observation`s.

**Verbs.** Set `Step.seq` to a number of your choosing and the verb's outcome comes back as a
`VerbResult` with the same `seq` in `Observation.verb_results`:

```json
{"seq":3,"outcome":"Accepted"}
{"seq":4,"outcome":"Refused","reason":"no item in slot 23 to move"}
```

- `Accepted` — eqoxide handed the verb to the game. It is *not* a claim the game did it: the server
  can still refuse (a bad target, out of range), and what actually happened shows in later
  `Observation`s and in the event feed.
- `Refused(reason)` — eqoxide did not act and nothing was sent to the server; the reason says why
  (net thread dead, character not dead, blank chat text, empty source slot, a queued move/pet
  command not yet drained, out-of-range gem, not implemented, queue full).
- Each verb with a `seq` is answered **exactly once**, in the `Observation` built on the tick that
  dispatched it (or the next one, for a verb refused at intake: queue full, or a `seq` sent with no
  verb). A verb queued behind others is answered up to one tick per verb ahead of it later. A `Step`
  with no `seq` gets no answer; `seq` values are yours to choose and only need to be distinct among
  your unanswered verbs.
- The host keeps at most 64 undelivered results per connection (`MAX_QUEUED_RESULTS`); beyond that
  the oldest is discarded. A client that reads its `Observation`s never hits this.
- Verbs are not cancelled by zone changes or death: the queue is per connection and nothing flushes
  it. A verb dispatched while the character is dead or mid-zone is gated only by the checks in the
  table above (the respawn check, a dead net thread) and otherwise handed to the game as usual, so
  re-check `own`/`dead` after a zone change rather than assuming a queued verb still makes sense.

**Calls.** The reply to `Call{id}` is a `CallReply{id, status, body}` in `Observation.call_replies`,
in the order calls finished since the previous `Observation`. `body` is the op's result for `Ok` and
`Accepted`, and a reason string for every other status. Exactly one reply per call.

### `own: OwnState`

```rust
pub struct OwnState {
    pub pos: [f32; 3],
    pub heading: f32,
    pub hp: i32,
    pub hp_max: i32,
    pub hp_verified: bool,        // false during the estimate-only window before the first real OP_HPUpdate
    pub mana: i32,
    pub mana_max: i32,
    pub endurance: i32,
    pub endurance_max: i32,
    pub endurance_confirmed: bool, // same "not a confident guess" contract as hp_verified
    pub casting: Option<CastingView>,
    pub buffs: Vec<BuffView>,      // always present (empty = none), sorted by slot
    pub zone_name: String,
    pub target_id: Option<u32>,
    pub target_name: Option<String>,
    pub auto_attack: bool,
    pub sitting: bool,
    pub held: bool,                // true iff the local controller is frozen — movement Steps are silently dropped while this is true
    pub player_class: String,
    pub player_level: u32,
}
```

`held` is the honesty mechanism for movement, which has no per-`Step` answer: an agent sending
movement into a frozen controller would otherwise get zero signal that its input is being dropped.
Check `held` and decide whether to keep trying or do something else, rather than assuming a `Step`
you sent took effect.

`CastingView { spell_id: u32, elapsed_ms: u32, cast_ms: u32 }`, present while a cast is in flight.

`BuffView { slot: u32, spell_id: u32, duration_ticks: i32 }` — `duration_ticks` stays **signed** on
the wire: EQEmu writes `-1000` for a permanent buff, and treating that as unsigned would report a
fabricated-looking ~4.29 billion ticks instead.

### `visible: Vec<VisibleEntity>`

```rust
pub struct VisibleEntity {
    pub spawn_id: u32,
    pub name: String,
    pub is_npc: bool,
    pub level: u32,
    pub race: String,
    pub pos: [f32; 3],
    pub heading: f32,
    pub hp_pct: f32,
    pub dead: bool,
}
```

Filtered each tick from the character's actual position: a distance cutoff (`VISIBILITY_DIST` =
500.0, mirroring the renderer's own draw distance) plus a real line-of-sight occlusion check against
zone collision geometry (`eqoxide-agent-vision-filter`) — this is not the full zone's entity list.
Sorted ascending by `spawn_id` so the same entity set produces the same wire order on every tick,
regardless of the underlying `HashMap`'s iteration order.

### `legal_actions: LegalActionMask`

```rust
pub struct LegalActionMask {
    pub gems: [bool; 9],              // which of the 9 spell gem slots hold a memorized, castable spell
    pub abilities: Vec<AbilityFeature>,
}

pub struct AbilityFeature {
    pub gem: u8,
    pub spell_id: u32,
    pub target_type: u8,      // raw EQ target-type code
    pub effects: Vec<i32>,    // raw SPA effect ids (SPA_BLANK slots already filtered out)
    pub mana_cost: i32,       // signed — some clicks/procs cost negative mana
    pub cast_time_ms: u32,
    pub recast_time_ms: u32,
}
```

Built from `GameState.mem_spells` + the spell database — one `AbilityFeature` per non-empty gem, so
`gems[i] == true` implies (barring an unrecognized spell id) an `abilities` entry with `gem == i`.

## Tick cadence and timing

| Constant | Value | Meaning |
|---|---|---|
| Tick period | 150ms | Mirrors `eqoxide-net`'s own decision cadence (`NAV_TICK_MS`) — movement/combat decisions don't actually change faster than this today, so ticking the socket faster would just repeat stale decisions. |
| `MOVE_LATCH` | 300ms (2x tick) | Deadline on each tick's re-application of the latched movement; if the host stops ticking or the connection ends, the character stops within this long. |
| Handshake timeout | 5s | How long the server waits for `Hello` before giving up and closing the connection. |
| Write timeout | 450ms (3x tick) | How long a per-tick `Observation` write may block before the server gives up on a non-draining client and closes the connection. |
| Max line size | 256 KiB | Cap on any single line (`Hello` or `Step`) with no trailing `\n`, to bound memory on a misbehaving client. |
| `MAX_QUEUED_VERBS` | 16 | Verbs waiting for a tick on one connection; more are refused. One verb is dispatched per tick. |
| `MAX_QUEUED_RESULTS` | 64 | Undelivered `VerbResult`s kept per connection; oldest dropped beyond that. |
| `MAX_CALLS_IN_FLIGHT` | 32 | Calls running at once on one connection; more are answered `Unavailable`. |

A stalled tick (slow write, a heavy line-of-sight pass) does not cause a burst of back-to-back
catch-up ticks once it clears — the loop just resumes on the normal cadence from whenever the stall
ended, so `tick`'s rate of increase stays an honest read of elapsed time rather than spiking after a
stall.

## Examples

`crates/eqoxide-agent-protocol/examples/fixed_sequence_client.rs` is the minimal client —
handshake, send a short scripted sequence of movement `Step`s, print each `Observation`.
`crates/eqoxide-agent-protocol/examples/verbs_and_calls_client.rs` exercises every wired verb with
`seq` correlation against `verb_results`, and several calls (including a refused one and an unknown
op) correlated by `id` against `call_replies`; its `cargo test` runs the same script against an
in-process fake host. Run either against a live instance:

```
cargo run -- --testzone --agent-socket /tmp/eqoxide-agent.sock &
cargo run -p eqoxide-agent-protocol --example verbs_and_calls_client -- /tmp/eqoxide-agent.sock [spawn_id]
```

(`--testzone`'s synthetic debug zone loads no collision geometry, so `visibility_available` stays
`false` and `pos`/`hp` stay at their zero defaults in that mode — that's expected, not a bug; use a
real EQEmu login to see live state.)
