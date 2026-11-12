# HTTP route relocation plan

Follow-up planning pass required by §9 and §16 of
[`2026-09-21-agent-harness-separation-design.md`](2026-09-21-agent-harness-separation-design.md):
map every `eqoxide-http` route to its Agent Plugin API equivalent, then move the HTTP layer to
[`eqoxide-agent-harness-example-http-astar`](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar)
(`harness-http`) and delete `crates/eqoxide-http` from eqoxide.

Decisions already made: the socket protocol is **extended** to carry everything the routes need (the
harness is a thin layer over `harness-socket`, with no privileged access), and the in-tree HTTP server is
**deleted** once the harness covers the surface (as the A* stack was in #1169).

Status: plan only on the eqoxide side. The harness side has a head start: draft PR
[`eqoxide-agent-harness-example-http-astar#24`](https://github.com/djhenry/eqoxide-agent-harness-example-http-astar/pull/24)
("Serve the agent control routes from harness-http") already serves every route the **current** protocol
can support (`move` goto/follow/stop/manual/jump, `combat` target/target-name/attack/consider/cast,
`interact` sit/stand, `lifecycle/respawn`, `observe` state/entities/nav/zone_lines, `/health`) over a
driver thread that owns the `harness-socket` link. It is unmerged and has not had a live run. Everything
else in the tables below is blocked on protocol work in eqoxide, which is what this plan sequences.

Two deliberate differences in #24 carry through to this plan: there is no `zone_cross` route (agents walk to
a point listed by `GET /v1/observe/zone_lines`), and `manual` cancels any nav goal.

## Why this is more than a copy

1. **`eqoxide-http` reads eqoxide's internals.** Handlers use `HttpState` (game_state, world tables,
   inventory, guild/merchant/quest state, command queues, event log). An external process cannot. Every
   read needs a socket-visible equivalent, and every write needs a verb.
2. **The honesty machinery has to cross the socket.** Many handlers answer 409/404/400 with a reason
   (refusals, "settle on the server echo" in #347/#1175, `require_live_session`, busy-queue refusals).
   `AgentVerb` today is fire-and-forget. Without a per-verb result carrying an accepted/refused
   outcome and reason, `harness-http` could only ever answer 200, which is exactly the falsehood class
   #1002 and #952 were filed against.
3. **Today's protocol is small.** `eqoxide-agent-protocol` carries a per-tick `Observation` (own state,
   vision-filtered `visible`, legal-action mask) plus `Combat{Target,Attack,Consider,Cast}`,
   `Interact{Sit,Stand}` and `Lifecycle::Respawn`. `Merchant`, `Inventory`, `Quests`, `Chat` are
   reserved uninhabited enums.
4. **Information parity (§12).** Some routes return things a player could not know (full entity list,
   `/observe/packets`, `/observe/debug`, `/observe/frame`). These need an explicit keep/drop decision
   rather than being ported by default.

## Protocol additions needed

| Addition | Purpose |
|---|---|
| **Verb result** (`Accepted` / `Refused{reason}` / later `Settled`) returned per verb | Preserve the refusal/echo semantics every mutating route has today. |
| **`Query` request / `QueryReply` response** (one-shot, id-correlated) | Cover the request/response `observe`-family reads that do not fit a per-tick push (inventory, spells, skills, doors, zone lines, who, item text, dialogue). |
| **`Event` stream** (pushed, cursor-based) | Replace `/v1/events/*` and `/v1/observe/messages`. |
| Filled-in verbs: `Merchant`, `Inventory`, `Quests`, `Chat`, plus new `Group`, `Guild`, `Social`, `Trainer`, `Pet`, `Lifecycle{Camp,Exit}`, `Interact{Hail,Say,Loot,Give,ClickDoor,Read,Dialogue,Run,Walk}`, `Move{Follow,Jump}` | The mutating routes. |

## Route mapping

Legend: **covered** = exists on the socket today; **verb** = needs a new `AgentVerb`; **query** = needs a
`Query`; **event** = needs the event stream; **decide** = information-parity / scope decision first.

### `/v1/combat`
| Route | Target |
|---|---|
| `POST attack`, `DELETE attack` | covered (`Combat::Attack`) + verb result |
| `POST target` | covered (`Combat::Target`) + verb result |
| `POST target/name` | harness-side: resolve name against `Observation.visible`, then `Target` (done in harness #24) |
| `POST consider` | covered (`Combat::Consider`) + verb result |
| `POST cast` | covered (`Combat::Cast`) + verb result |
| `POST memorize`, `POST scribe` | verb (`Combat::Memorize`, `Combat::Scribe`) |

### `/v1/move`
| Route | Target |
|---|---|
| `POST move` (goto/follow/stop) | harness-side (`harness-nav` walker over `Step` movement); done in harness #24. `follow` here is the harness walking to the target, not the native-fidelity `MoveVerb::Follow` of design §7.1, which is still unbuilt |
| `POST manual` | covered by `Step` movement payload; done in harness #24 |
| `POST jump` | done in harness #24 via the movement payload |
| `GET zone_lines` (replaces `zone_cross`) | done in harness #24 |

### `/v1/interact`
| Route | Target |
|---|---|
| `sit`, `stand` | covered (`Interact::Sit/Stand`) |
| `hail`, `say`, `loot`, `give`, `click_door`, `read`, `dialogue`, `run`, `walk` | verb each (`hail`/`loot` "nearest" resolves harness-side from `Observation`, which removes the #1002 camera-focus coupling for good) |

### `/v1/lifecycle`, `/v1/chat`, `/v1/social`, `/v1/group`, `/v1/guild`, `/v1/quests`, `/v1/trainer`, `/v1/pet`, `/v1/merchant`, `/v1/inventory`
| Group | Routes | Target |
|---|---|---|
| lifecycle | `respawn` | covered |
| lifecycle | `camp`, `exit` | verb (`Lifecycle::Camp/Exit`) |
| lifecycle | `friends` (GET/POST) | query + verb |
| chat | `tell`, `ooc`, `shout`, `group`, `guild` | verb (`Chat::*`) |
| social | `roster`, `invite`, `accept`, `remove` | query + verb |
| group | `roster`, `invite`, `accept`, `decline`, `leave`, `kick`, `makeleader` | query + verb |
| guild | `roster`, `invite`, `accept`, `leave`, `remove` | query + verb |
| quests | `log`, `completed`, `offers` | query |
| quests | `accept`, `decline`, `cancel` | verb (`Quests::*`) |
| trainer | `open`, `list`, `train`, `close` | verb + query |
| pet | `command` | verb (`Pet::Command`) |
| merchant | `open`, `close`, `buy`, `sell`; `list` | verb (`Merchant::*`); query |
| inventory | `move` | verb (`Inventory::Move`) |

### `/v1/observe`, `/v1/events`, `/v1/camera`
| Route | Target |
|---|---|
| `entities` | covered by `Observation.visible` (vision-filtered); the unfiltered form is **decide** |
| `inventory`, `spells`, `skills`, `doors`, `zone_entrances`/`zone_points`, `zone_exits`, `item_text`, `who`, `dialogue` | query |
| `messages`, `events/all`, `events/:category` | event |
| `debug`, `frame`, `packets`, `asset_sync` | **decide** (debug-only: expose behind an explicit debug query, or drop) |
| `camera` GET/POST, `camera/reset` | **decide** (render-only; recommend dropping from the harness and from eqoxide's programmatic surface, see #1002) |

## Sequence

Each step is its own PR (or PR pair) and leaves `main` green with the HTTP server still working until the
last step.

1. **Verb result.** Add the per-verb result to `eqoxide-agent-protocol` and the plugin host, with the
   refusal reasons the existing handlers use. Wire the already-covered verbs through it. This is the
   keystone: every later slice depends on it.
2. **`Query` + `Event` channels** in protocol and host, with golden wire-shape tests (the repo's existing
   convention in `verb.rs`).
3. **Read-only slices:** quests, inventory/spells/skills/doors/zone lines/who/item text, then events and
   messages.
4. **Mutating slices**, one group at a time in this order (smallest surface and best-tested first):
   chat, lifecycle, inventory move, interact, merchant, trainer, pet, quests, group, social, guild.
5. **Resolve the "decide" rows** with the user before porting or dropping them.
6. **Port to `harness-http`** in the harness repo, building on #24 (not from `GET /health`), one group per
   PR, each pinned to the eqoxide rev that provides its verbs (`docs/scope.md` "Bumping the eqoxide pin").
   Move the corresponding route tests with them; the existing `req_form` refusal guard and `testkit` are
   the starting point. Before the first of these, finish #24 itself: review it and give it the live run its
   test plan still lists as unchecked.
7. **Delete `crates/eqoxide-http`** from eqoxide and its wiring (`eqoxide-crash`, `eqoxide-command`,
   `eqoxide-core` references, workspace manifest), update `docs/http-api.md` to point at the harness repo.

## Open decisions for the owner

- Drop or keep the information-parity "decide" rows (`debug`, `frame`, `packets`, `asset_sync`, unfiltered
  `entities`).
- Drop the camera routes, or keep camera control as a debug-only verb.
- Whether `harness-http` may stay URL-compatible with `/v1/*` (so existing agent scripts keep working) or
  take the chance to change shapes.
