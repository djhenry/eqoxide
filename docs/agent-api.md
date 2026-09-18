# eqoxide Agent Plugin API

A low-latency, socket-based alternative to the [HTTP API](http-api.md), purpose-built to drive a
tight agent control loop (e.g. an RL policy) rather than a scripted/tool-calling agent. Where the
HTTP API is request/response and covers the client's full feature surface, the Agent Plugin API is
a persistent duplex connection carrying exactly two message shapes — `Step` in, `Observation` out —
at a fixed tick cadence. See `docs/specs/2026-09-15-agent-plugin-api-design.md` for the design
rationale; this document is the wire reference for a consumer implementing a client against it.

The wire types live in the `eqoxide-agent-protocol` crate, which has **zero dependency on any other
eqoxide crate** — it's what an external agent project pins directly. The in-client server lives in
`eqoxide-agent-plugin-host`.

## Enabling it

The API is **off by default**. Pass `--agent-socket <PATH>` on the eqoxide command line to bind it
to a Unix domain socket at that path:

```
eqoxide --agent-socket /tmp/eqoxide-agent.sock
```

There is no default path and no HTTP-style auto-scanned port — an unauthenticated local
control-plane socket must be something you opt into, not something every launch exposes. The
socket file is created with `0600` permissions (readable/writable only by the user that launched
eqoxide); if a stale file already exists at that path, eqoxide removes it and logs a warning before
binding.

## Connecting

The socket serves **one connection at a time** (spec §10) — right for a single-character session.
Connecting while another client is attached simply queues behind it at the OS accept-queue level;
there's no separate multiplexing.

### Framing

Every message, in both directions, is one line of JSON terminated by `\n` (NDJSON) —
`eqoxide_agent_protocol::framing::encode_line` / `decode_line`. There is no length prefix; a reader
that splits on `\n` (e.g. `BufRead::read_line`, tokio's `AsyncBufReadExt::lines()`) is sufficient.

### Handshake

The client sends `Hello` first, always:

```json
{"protocol_version": 1}
```

The server replies with one `HandshakeReply` line:

```json
{"status": "Accepted"}
```

or, on a version mismatch or a malformed `Hello`:

```json
{"status": "Rejected", "server_protocol_version": 1, "message": "client protocol_version 0 != server 1"}
```

On `Rejected`, or on any handshake failure (malformed line, EOF, or a 5-second timeout waiting for
`Hello`), the server closes the connection immediately — reconnect and try again rather than
sending anything further on that socket. `PROTOCOL_VERSION` (currently `1`) bumps whenever a wire
type changes shape in a way that breaks an old client; pin the version you were built against and
check it.

Once handshake succeeds, the server begins the tick loop described below. Nothing about the
connection is itself session state — the character/game state is the one shared, persistent thing,
so a disconnect-and-reconnect resumes against the same running character.

## Sending `Step`

At any point after a successful handshake, send zero or more `Step` lines:

```rust
pub struct Step {
    pub movement: Option<AgentMovement>,
    pub verb: Option<AgentVerb>,
}
```

Both fields are independent — a `Step` can carry movement only, a verb only, both, or (rarely)
neither. There is no per-`Step` acknowledgment; `Observation` is the only outbound message shape,
so the effect of a `Step` shows up in the next tick's `Observation`, not a direct reply.

### `movement` — continuous, held-key-style

```rust
pub struct AgentMovement {
    pub dir: [f32; 2],           // world (east, north); any magnitude — NOT normalized server-side
    pub up: f32,                 // -1..1, only affects swimming/climbing (auto-detected, not agent-selected)
    pub jump: bool,
    pub wish_heading: Option<f32>,  // facing override; None = derive heading from `dir`
}
```

`dir = [0.0, 0.0]` means "stand in place" (useful for a jump-with-no-movement `Step`). The eqoxide
side does **not** normalize `dir` for you (this mirrors the HTTP API's own `/v1/move/manual`;
normalization, if any, is render-side) — send a unit vector if that's what you want applied.

The server latches the last `Step` with non-`None` movement and **re-applies it every tick** for as
long as it keeps arriving — the same "held key" semantics as `ManualMove` elsewhere in the client.
If the agent goes quiet, movement naturally stops within ~300ms (`MOVE_LATCH`, ~2x the tick period)
rather than continuing forever on the last value received.

`wish_heading` exists because melee requires facing the target server-side; it can be set
independently of `dir`, e.g. to strafe sideways while still facing a target.

### `verb` — discrete, one-shot

```rust
#[serde(tag = "verb", content = "data")]
pub enum AgentVerb {
    Combat(CombatVerb),
    Interact(InteractVerb),
    Lifecycle(LifecycleVerb),
    Move(MoveVerb),
    Merchant(MerchantVerb),     // reserved, uninhabited — cannot be constructed yet
    Inventory(InventoryVerb),   // reserved, uninhabited
    Quests(QuestsVerb),         // reserved, uninhabited
    Chat(ChatVerb),             // reserved, uninhabited
}
```

Unlike `movement`, a verb fires **exactly once** — the tick that drains it — even though the
underlying `Step` object may still be the "latest" one for several more ticks while its `movement`
half keeps re-applying. A single `Combat::Cast` does not re-fire every 150ms.

| Variant | JSON shape | Dispatches to |
|---|---|---|
| `Combat::Target { spawn_id }` | `{"verb":"Combat","data":{"action":"Target","spawn_id":42}}` | `CommandState::request_target` |
| `Combat::Attack { on }` | `{"verb":"Combat","data":{"action":"Attack","on":true}}` | `CommandState::request_attack` |
| `Combat::Consider { spawn_id }` | `{"verb":"Combat","data":{"action":"Consider","spawn_id":7}}` | `CommandState::request_consider` |
| `Combat::Cast(CastRequest)` | `{"verb":"Combat","data":{"action":"Cast","gem":0,"target_id":42,"item_slot":null}}` | `CommandState::request_cast` |
| `Interact::Sit` | `{"verb":"Interact","data":{"action":"Sit"}}` | `request_sit(true)` |
| `Interact::Stand` | `{"verb":"Interact","data":{"action":"Stand"}}` | `request_sit(false)` |
| `Lifecycle::Respawn` | `{"verb":"Lifecycle","data":{"action":"Respawn"}}` | `request_respawn()` (no accepted/refused signal — check the next `Observation`) |
| `Move::ZoneCross` | `{"verb":"Move","data":{"action":"ZoneCross"}}` | **reserved** — accepted, currently a no-op |

`CastRequest` mirrors `eqoxide_ipc::CastRequest`: `{ gem: u8, target_id: Option<u32>, item_slot:
Option<u32> }`.

`Merchant`/`Inventory`/`Quests`/`Chat` are reserved for future scope — their enums have no variants
today, so no JSON value can ever deserialize into them; sending an `AgentVerb` tagged with one of
those names is a malformed `Step`, not a silent no-op.

### Malformed input

A `Step` line that fails to parse rejects just that one `Step` — the connection stays open, since
there's no per-`Step` ack to carry an error back on. A line that exceeds 256KB with no terminating
`\n` is treated as a client bad enough to disconnect outright (the same cap applies to the
handshake's `Hello` line).

## Receiving `Observation`

The server pushes one `Observation` line **every tick, unconditionally** — independent of whether a
`Step` arrived that tick. There is no throttling or backpressure-driven skip; if you stop reading,
the socket write blocks and, after roughly 3 ticks' worth of delay, the server gives up and closes
the connection rather than wedging forever.

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
}
```

- **`tick`** — starts at 0 on the first `Observation` after handshake, increments once per loop
  iteration that reached the point of building an `Observation` (not per successful write — a
  failed write ends the connection instead of retrying). Use it to detect dropped/bursted delivery
  and reconstruct elapsed time; there's no per-`Step` ack to hang timing off of otherwise.
- **`dead`** — the character is dead as ongoing *state*, not a terminal boundary. This is routine in
  combat training: the agent keeps receiving observations while dead and must send
  `Lifecycle::Respawn` to continue.
- **`terminated`** — true session end (the net thread has died for good — e.g. disconnected from
  the EQ server). eqoxide only ever asserts this when it can back it up.
- **`truncated`** — always `false` from eqoxide's side in v1; reserved for an external harness that
  wants to impose its own rollout cutoff without needing eqoxide to say so.
- **`visibility_available`** — false whenever no zone collision geometry is loaded yet (startup,
  zone transitions, or the synthetic debug zone `--testzone` builds). In that window `visible` is
  always `[]` regardless of what's actually nearby — that's *not* the same claim as "nothing is
  visible," so check this before trusting an empty `visible` list.

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

`held` is the honesty mechanism for a case the HTTP API instead refuses pre-emptively
(`require_live_session`/`MoveGate`): since this API has no per-`Step` ack, an agent sending movement
into a frozen controller would otherwise get zero signal that its input is being dropped. Check
`held` and decide whether to keep trying or do something else, rather than assuming a `Step` you
sent took effect.

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
| `MOVE_LATCH` | 300ms (2x tick) | How long a latched `movement` keeps being re-applied after the last `Step` that set it, before it lapses. |
| Handshake timeout | 5s | How long the server waits for `Hello` before giving up and closing the connection. |
| Write timeout | 450ms (3x tick) | How long a per-tick `Observation` write may block before the server gives up on a non-draining client and closes the connection. |
| Max line size | 256 KiB | Cap on any single line (`Hello` or `Step`) with no trailing `\n`, to bound memory on a misbehaving client. |

A stalled tick (slow write, a heavy line-of-sight pass) does not cause a burst of back-to-back
catch-up ticks once it clears — the loop just resumes on the normal cadence from whenever the stall
ended, so `tick`'s rate of increase stays an honest read of elapsed time rather than spiking after a
stall.

## Minimal example

`crates/eqoxide-agent-protocol/examples/fixed_sequence_client.rs` is a complete, minimal client —
handshake, send a short scripted sequence of `Step`s, print each `Observation`. Run it against a
live instance:

```
cargo run -- --testzone --agent-socket /tmp/eqoxide-agent.sock &
cargo run -p eqoxide-agent-protocol --example fixed_sequence_client -- /tmp/eqoxide-agent.sock
```

(`--testzone`'s synthetic debug zone loads no collision geometry, so `visibility_available` stays
`false` and `pos`/`hp` stay at their zero defaults in that mode — that's expected, not a bug; use a
real EQEmu login to see live state.)
