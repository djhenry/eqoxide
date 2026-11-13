# Typed Ops: Replacing the HTTP Vocabulary in `eqoxide-agent-ops` — Design

**Date:** 2026-11-13
**Status:** Proposed (not implemented)

Wire reference: `docs/agent-api.md`. Op reference: `docs/agent-ops.md`. Background:
`docs/specs/2026-11-13-agent-api-verbs-and-calls-design.md`.

## 1. Problem

`eqoxide-agent-ops` serves every agent operation that is not a verb. Nothing in it speaks HTTP any more,
yet the interface is still HTTP-shaped:

- An op is named `"POST /v1/merchant/buy"`. The method and the `/v1` prefix carry no meaning on the
  socket. `Ops::add_prefix` exists only to emulate a path parameter (`GET /v1/events/<name>`).
- Arguments are `{"query": {...}, "body": {...}}`. `Req::query` renders JSON to a query string and
  parses it back (`serde_urlencoded`, `form_urlencoded`) so that `5` and `"5"` both work. `req_form.rs`
  (792 lines) exists to police which mix of query and body each handler accepts.
- A refusal is a numeric status (`wire::StatusCode`, ten constants) that `dispatch.rs` maps back onto
  `CallStatus`. Every handler picks a code, and the harness maps the code to HTTP again.
- Every caller must know the op strings and argument shapes. Nothing in the protocol crate types them.

The cost is concrete. The argument and status plumbing (`wire.rs`, `req_form.rs` and `dispatch.rs`) is about 1,200 of the crate's 8,000 lines, a typo in an
op name fails at runtime as `NotFound`, and the harness duplicates the route table that the ops crate
already holds.

## 2. Goals and non-goals

Goals:

- One name per op, with no HTTP method or path.
- One argument object per op, defined as a Rust type that both ends share.
- One error type with the five outcomes `CallStatus` already has, replacing numeric status codes.
- Keep the `eqoxide-agent-protocol` rule that it depends on no other eqoxide crate.

Non-goals:

- No change to verbs, `Step`, `Observation` or the tick cadence.
- No change to what any op does. A refusal stays a refusal, and the honesty rules from #952, #1002 and
  #347 hold.
- Preview mode keeps its own HTTP server.

## 3. Design

### 3.1 Op names and arguments

An op name is `"<group>.<action>"`, for example `merchant.buy`, `observe.skills` and
`events.chat`. `events.<name>` becomes an `events.read` op with a `name` argument, so `add_prefix`
and `Req::tail` are deleted.

`Call.args` becomes one JSON object. The `query` and `body` split is deleted. A read that took
query parameters takes the same fields at the top level of `args`.

### 3.2 Typed requests in the protocol crate

The protocol crate gains an `Op` enum with one variant per op, each carrying its argument struct:

```rust
#[serde(tag = "op", content = "args")]
pub enum Op {
    #[serde(rename = "merchant.buy")]
    MerchantBuy(MerchantBuy),
    // ...
}
```

`Call` becomes `Call { id, op: Op }`. A malformed argument fails at deserialization with a path to the
field, and the host answers `Invalid` before any handler runs. This removes `Req::json`,
`optional_json`, `query` and the `req_form` guard.

Reply bodies stay `serde_json::Value` in this design. Typing replies means moving the snapshot types
(`Spells`, `Skills`, the roster and quest log types) out of `eqoxide-core` into the protocol crate.
That move is correct but large, and the request side delivers the main simplification without it.

### 3.3 One error type

Handlers return `Result<Value, OpError>`:

```rust
pub enum OpError { Invalid(String), NotFound(String), Conflict(String), Unavailable(String), Failed(String) }
```

`OpError` maps one to one onto `CallStatus`. A queued-but-unconfirmed action returns
`Ok(Accepted { request_id })`, which maps to `CallStatus::Accepted`. `StatusCode`, `Reply`,
`IntoReply` and `dispatch::map_status_code` are deleted. `refusal.rs` keeps its single polarity site,
retargeted from status codes to `OpError`.

### 3.4 Harness

The harness keeps its HTTP routes and maps `CallStatus` to a response code in one table. Its
`proxy::forward` stops being a path-to-string forwarder and builds an `Op` value per route. Its pin
moves with the eqoxide revision, as it does today.

## 4. Wire compatibility

This is a breaking change to `Call`, so `PROTOCOL_VERSION` goes from 5 to 6. There is no alias for the
old `"POST /v1/..."` names. The harness is the only client, and both repositories pin together, so an
alias would be dead code the day it merges. The handshake already rejects a mismatched version.

## 5. Sequence

1. **Ops crate internals.** Introduce `OpError` and `Result` returns, convert handlers group by group,
   then delete `StatusCode`, `Reply`, `IntoReply` and the status mapping. No wire change.
2. **Argument types.** Move each group's request structs into the protocol crate behind the `Op` enum.
   Delete `Req`, the query rendering, `req_form.rs`, `serde_urlencoded` and `form_urlencoded`.
3. **Wire cut.** Switch `Call` to the `Op` enum, rename ops, bump `PROTOCOL_VERSION` to 6 and update
   `docs/agent-api.md`, `docs/agent-ops.md` and the example client.
4. **Harness.** Pin the new revision, build `Op` values in `proxy::forward`, and replace the
   status-code table with a `CallStatus` table.

Steps 1 to 3 are one eqoxide PR each. Step 4 follows in the harness repository.

## 6. Risks

- **Protocol crate growth.** It gains every request struct, around 60 types. Each is plain `serde` with
  no dependency, so the no-eqoxide-dependency rule holds. Version bumps become more frequent because
  any argument change is a wire change.
- **Behaviour drift in step 1.** Handlers pick status codes with care, and the tests assert them. The
  conversion keeps those tests and changes only the assertion form, so a wrong mapping fails a test.
- **Argument leniency.** `Req::query` accepts `5` and `"5"` today. Typed arguments accept only the
  declared type. Agents that send numbers as strings break, and this is the intended tightening.

## 7. Open decisions

- Whether to type replies (§3.2) in the same change or leave them for a later one. This design leaves
  them for later.
- Whether read ops with no arguments become unit variants of `Op` or share one `Query` enum. A single
  `Op` enum is simpler and is the proposal here.
