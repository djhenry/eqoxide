# Agent Ops: Shape in the Protocol Crate, Handling in the Plugin Host — Design

**Date:** 2026-11-16
**Status:** Approved 2026-11-16, implementation in progress

Wire reference: `docs/agent-api.md`. Op reference: `docs/agent-ops.md`. Background:
`docs/specs/2026-11-13-agent-api-verbs-and-calls-design.md`.

## 1. Problem

Three crates describe one agent interface:

- `eqoxide-agent-protocol` defines the verbs, `Step`, `Observation` and `Call`. It has no eqoxide
  dependency, so the external harness can pin it alone.
- `eqoxide-agent-plugin-host` binds the socket, runs the session and handles the verbs
  (`session.rs`, 1,847 lines).
- `eqoxide-agent-ops` handles every other operation (8,000 lines). `main.rs` builds an `ApiDispatcher`
  from it and passes that dispatcher into the plugin host.

The ops crate and the plugin host have the same lower dependencies (`core`, `ipc`, `command`,
`zone-geometry`) and write the same request slots. They also copy each other's rules. `session.rs`
repeats the gem bound from `combat.rs` and the dead-session refusal from `lib.rs`, each marked "mirrors
`eqoxide-agent-ops`". The interface of the ops crate is also still HTTP-shaped. An op is named
`"POST /v1/merchant/buy"`, arguments are a `query` and `body` pair parsed through `serde_urlencoded`, and
a refusal is one of ten numeric `StatusCode` values that `dispatch.rs` maps back onto `CallStatus`.

## 2. Design

Two crates, split by what each one is.

**`eqoxide-agent-protocol` owns the shape of every operation.** It gains an `Op` enum with one variant per
op, each carrying a plain `serde` argument struct, and an `OpError` type. It stays free of eqoxide
dependencies, so the harness pins the same crate it pins today.

**`eqoxide-agent-plugin-host` owns the handling.** The handlers from `eqoxide-agent-ops` move into it
group by group (`merchant`, `quests`, `group`, `guild`, `trainer`, `observe`, `interact`, `combat`,
`social`, `events`). The verb handler in `session.rs` and the op handlers then share one state bundle
and one set of refusal rules. `eqoxide-agent-ops` and `ApiDispatcher` are deleted.

### 2.1 Operation shape

An op name is `"<group>.<action>"`, such as `merchant.buy`. `events.<name>` becomes `events.read` with a
`name` argument, so `add_prefix` and `Req::tail` are deleted. `Call` becomes `Call { id, op: Op }`:

```rust
#[serde(tag = "op", content = "args")]
pub enum Op {
    #[serde(rename = "merchant.buy")]
    MerchantBuy(MerchantBuy),
    // one variant per op
}
```

`Call.args` is one object. The `query` and `body` split is deleted. A malformed argument fails at
deserialization with a path to the field, and the host answers `Invalid` before any handler runs. That
removes `Req`, the query-string round trip, `serde_urlencoded`, `form_urlencoded` and the 792-line
`req_form.rs`.

### 2.2 One error type

Handlers return `Result<Value, OpError>`:

```rust
pub enum OpError { Invalid(String), NotFound(String), Conflict(String), Unavailable(String), Failed(String) }
```

`OpError` maps one to one onto `CallStatus`. A queued action that the game has not confirmed returns an
`Accepted { request_id }` value, which maps to `CallStatus::Accepted`. `StatusCode`, `Reply`, `IntoReply`
and the status mapping are deleted. `refusal.rs` keeps its single polarity site, retargeted onto
`OpError`.

### 2.3 Reply bodies

Reply bodies stay `serde_json::Value` in this refactor. A handler builds a body with a `json!` literal
(about 75 across the ops code) or serializes a state struct from `eqoxide-core` or `eqoxide-ipc`, such as
`ActiveTask`, `GroupMemberView`, `MerchantSnapshot` or `DoorView`. The protocol crate cannot depend on
those crates, so typing replies means moving those structs or defining protocol-side copies with
conversions. That choice is a separate change that follows this refactor.

### 2.4 Discovery

The `Op` enum is the catalogue for a Rust plugin. A runtime `describe` op for plugins in other
languages is deferred and reassessed after reply bodies are typed.

## 3. Wire compatibility

`Call` changes, so `PROTOCOL_VERSION` goes from 5 to 6. There is no alias for the old `"POST /v1/..."`
names. The harness is the only client and both repositories pin together, so an alias would be dead code
on the day it merges. The handshake already rejects a mismatched version.

## 4. Sequence

1. **Move.** Move `eqoxide-agent-ops` into `eqoxide-agent-plugin-host` as a module tree, then delete the
   crate. This changes no behaviour and no wire format. Repoint the `app.rs` use of `CONN_STALE_SECS`,
   the `main.rs` start-up call, and the `eqoxide-net` dev-dependency that reads two constants.
2. **Merge state.** Replace `OpState` and the session state with one bundle, and delete the "mirrors
   `eqoxide-agent-ops`" duplicates in `session.rs` in favour of one shared rule each.
3. **Errors.** Introduce `OpError`, convert handlers group by group, then delete `StatusCode`, `Reply` and
   `IntoReply`.
4. **Shape.** Move the argument structs into the protocol crate behind `Op`, switch `Call`, rename the ops,
   bump `PROTOCOL_VERSION` to 6 and update `docs/agent-api.md`, `docs/agent-ops.md` and
   the example client.
5. **Harness.** Pin the new revision, build `Op` values in `proxy::forward`, and replace the status-code
   table with a `CallStatus` table.

Steps 1 to 4 are one eqoxide PR each. Step 5 follows in the harness repository.

## 5. Risks

- **Plugin host size.** It grows from about 2,600 to about 10,000 lines. The module tree keeps each group in
  one file, and step 1 changes no code inside the handlers.
- **Test coverage during the move.** The ops tests build `OpState` through `testkit.rs`. Step 1 moves the
  fixture with the handlers so the same tests run before and after.
- **Protocol crate growth.** It gains about 60 argument structs. Each is plain `serde`, so the
  no-eqoxide-dependency rule holds. Any argument change is now a wire change and bumps the version.
- **Argument leniency.** `Req::query` accepts `5` and `"5"` today. Typed arguments accept only the declared
  type, so an agent that sends numbers as strings breaks. This is the intended tightening.

## 6. Deferred

- Typing reply bodies, which follows this refactor.
- A `describe` op, reassessed once reply bodies are typed.
