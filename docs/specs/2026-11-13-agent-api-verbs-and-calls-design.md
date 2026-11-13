# Agent API: Verbs, Calls and Answers — Design Record

**Date:** 2026-11-13
**Status:** Implemented (record of what shipped; deferrals in §7)

Wire reference: `docs/agent-api.md`. Op reference: `docs/agent-ops.md`. Route-by-route sequencing
of the move off HTTP: `docs/specs/2026-11-12-http-route-relocation-plan.md`.

## 1. Goal

An agent should be able to do vendor, inventory, quest and chat work over the agent socket alone, with
no HTTP server in eqoxide, and without losing the honesty the HTTP routes had: an agent must be told
when a request was refused and why, never given a cheerful "ok" for something that did not happen
(the #952/#1002/#347 falsehood class). Reads must not bloat the per-tick `Observation`.

## 2. Principle: the same information as a player

The agent sees what a player at the keyboard would see, no more. `Observation.visible` is distance- and
line-of-sight-filtered (not the zone's entity list); the unfiltered form and the render-only debug,
frame, packets, asset_sync and camera routes were dropped rather than ported. What a player reads by
opening a window (bag, vendor, quest journal, chat log, who list) is read by an agent with a call, on
demand, from the same snapshots the client renders. Likewise every verb is something a player can do
with the keyboard or UI; eqoxide does not do the player's thinking (auto-chase and auto-face melee
live in the harness, not the client).

## 3. Two shapes: verbs and calls

| | Verb | Call |
|---|---|---|
| Carried in | `Step.verb` (one per `Step`) | `Step.calls` (any number) |
| Typed | One enum value (`AgentVerb`) | `op` string plus `{"query","body"}` JSON |
| Runs | Queued FIFO, one dispatched per 150 ms tick | Own task, concurrently, to completion |
| Answered by | `Observation.verb_results`, matched by `seq` | `Observation.call_replies`, matched by `id` |
| Answer | `Accepted` or `Refused(reason)` | `CallStatus` plus a body (data, or a reason) |
| Right for | Fire-and-settle actions on the control-loop cadence | Reads, and anything that waits on the server |

The split is by what the operation needs, not by topic. A verb must be cheap, complete within one tick
and need no reply body. A merchant purchase waits for the server's echo (seconds); a quest turn-in
waits on the NPC; every read returns data. All three are calls, so a slow call never delays the verbs
behind it or the tick loop. Handlers are the native ops in `eqoxide-agent-ops` (they read `OpState`:
the honesty machinery, `require_live_session`, busy-queue and settle-on-echo refusals), run through
`ApiDispatcher::dispatch`, which maps the op's reply code to a `CallStatus` so a refusal is never
`Ok`. Reimplementing those handlers in the harness was rejected: an external process cannot see
`OpState`.

### Verb set and op set per requested group

| Brief item | Where it is today |
|---|---|
| Merchant open / close / buy / sell / list | Calls: `POST /v1/merchant/{open,close,buy,sell}`, `GET /v1/merchant/list`. No verb (`MerchantVerb` is reserved and uninhabited). |
| Inventory move | Verb `Inventory::Move{from,to}`. |
| Inventory equip / unequip | Same verb: worn slots are 0-21, so a move into one is an equip. |
| Inventory read | Call `GET /v1/observe/inventory`. |
| Inventory destroy, consume | Not available (§7). |
| Loot a corpse | Call `POST /v1/interact/loot`. Spawn state to find the corpse is in `Observation.visible` (`dead`). |
| Quests hail | Call `POST /v1/interact/hail` (and `say`, `dialogue`). |
| Quests turn-in | Call `POST /v1/interact/give` (hands an inventory slot to an NPC). |
| Quests accept / decline / cancel / read | Calls `POST /v1/quests/{accept,decline,cancel}`, `GET /v1/quests/{log,completed,offers}`. No verb (`QuestsVerb` reserved and uninhabited). |
| Chat send | Verbs `Chat::{Tell,Ooc,Shout,Group,Guild}`. |
| Chat receive | Calls `GET /v1/observe/messages[?kind=]`, `GET /v1/events/all`, `GET /v1/events/<category>`. Not pushed on the tick. |
| Pet, camp, exit, respawn, sit/stand, combat | Verbs. |

## 4. Observation stays lean

`Observation` is pushed every 150 ms for the life of the connection, so every byte on it is paid
~7 times a second whether or not the agent cares. It therefore carries only character state,
the vision-filtered `visible` list, the legal-action mask and the answers to earlier requests.
Inventory, merchant stock, quest journals, rosters and the chat/event log are fetched on demand with
a call, from the same snapshot slots the client already maintains (each reply carries
`snapshot_age_ms` where staleness matters). A policy that never trades never pays for trade data; a
policy that wants the log asks for it when it wants it. A push channel for chat or events was
considered and not built: `GET /v1/events/*` with a `since` cursor is cheap, gap-aware, and keeps the
tick line bounded.

## 5. The acknowledgment

Verbs used to be fire-and-forget; the old HTTP routes answered 4xx with a reason. Without an
equivalent the socket could only ever say "ok". The solution:

- `Step.seq` (caller-chosen) rides with the verb; the host echoes it in a `VerbResult` in the
  `Observation` built on the tick that dispatched the verb. Exactly one answer per sequenced verb;
  none if no `seq`.
- `VerbOutcome::Accepted` means "handed to the game", deliberately not "the server did it"; the
  server's actual response shows in later observations and the event feed. `Refused(reason)` means
  nothing was sent. Refusal reasons reuse the existing handlers' wording (dead net thread, respawn
  from a character not confirmed dead, blank chat text, empty source slot, a move or pet command
  already queued, gem out of range).
- Bounds: 16 verbs queued per connection (`MAX_QUEUED_VERBS`; overflow is refused, never silently
  displaced), 64 undelivered results (`MAX_QUEUED_RESULTS`), 32 calls in flight
  (`MAX_CALLS_IN_FLIGHT`; overflow answered `Unavailable`). No host-side deadline on calls: each op
  has its own (merchant 4 s, social/observe 6 s, give 8 s), plus the action watchdog (30 s) which
  records a terminal outcome for a mutating op in the event feed. Zone changes and death do not
  flush the verb queue; per-verb gating is the only state check.
- Calls get the same guarantee through `CallStatus` (`Ok`/`Accepted`/`Invalid`/`NotFound`/`Conflict`/
  `Unavailable`/`Failed`), derived from the op's reply code.
- Movement keeps the `held` flag on `OwnState` as its honesty signal, because movement is continuous
  and has no per-`Step` result.

## 6. Version history

| Version | PR | Change |
|---|---|---|
| 1 | #1131 | Initial socket API: `Step`/`Observation`, combat/interact/lifecycle verbs. |
| 2 | #1158 | `asset_cache_dir` on `Accepted`. |
| 3 | #1189 | `Step.seq`, `Observation.verb_results`. |
| 4 | #1190 | `Step.calls`, `Observation.call_replies`. |
| (4) | #1193, #1194 | `Chat` verbs; `Lifecycle::Camp/Exit`, `Inventory::Move`, `Pet::Command`. Shipped without a bump. |

A bump is required whenever a `Step` gains a field, because `Step` is `deny_unknown_fields` and an old
host would silently drop the whole line; the handshake turns that into a loud `Rejected`. Matching is
exact. The two verb additions after v4 did not bump: a client using them against an older v4 host
passes the handshake and has the line dropped. That is a compatibility gap; the next verb addition
should bump the version, and arguably those two should have.

## 7. Deferrals and gaps

- **Inventory destroy and consume** (and ground-drop): no verb and no op exist anywhere in
  `eqoxide-agent-ops` or the command queue. Not built; needs a client-side capability first.
- **Equip as its own verb:** unnecessary, `Inventory::Move` covers it. No auto-slot-finding.
- **Verb for merchant/quests:** `MerchantVerb` and `QuestsVerb` are reserved uninhabited enums in
  `verb.rs`, with `AgentVerb::Merchant`/`Quests` variants and golden tests asserting they reject
  every input. Those groups are calls, so the types are dead and are a **candidate for deletion**
  (with a protocol bump). They were left in place here because removing wire types was out of scope.
- **`Move::ZoneCross`:** reserved, answered `Refused("not implemented")`. Agents walk to a zone line
  listed by `GET /v1/observe/zone_entrances` / `zone_exits`. Also a deletion candidate.
- **Cancel a queued verb** or a running call: none.
- **Per-verb result for verbs sent without `seq`:** deliberately none.
- **Push of chat/events:** polling only (§4).
